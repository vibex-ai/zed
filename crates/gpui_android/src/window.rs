use crate::{events::ClickState, ime::ImeEvent};
use android_activity::AndroidApp;
use anyhow::Context as _;
use gpui::{
    AnyWindowHandle, Bounds, Capslock, Decorations, DevicePixels, DispatchEventResult, Edges,
    GpuSpecs, Modifiers, Pixels, PlatformAtlas, PlatformDisplay, PlatformInput,
    PlatformInputHandler, PlatformWindow, Point, PromptButton, PromptLevel, RequestFrameOptions,
    ResizeEdge, Scene, Size, TextInputStateChange, WindowAppearance, WindowBackgroundAppearance,
    WindowBounds, WindowControlArea, WindowControls, WindowDecorations, WindowInsets, WindowParams,
    px,
};
use gpui_wgpu::{GpuContext, WgpuRenderer, WgpuSurfaceConfig};
use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long after a long press the focus change it causes is still expected.
///
/// The menu takes focus in the deferred effect of the same frame, so this only
/// has to cover a slow frame or two.
const CONTEXT_MENU_KEYBOARD_HOLD: Duration = Duration::from_millis(500);

/// Wraps the current `ANativeWindow` so `WgpuRenderer` can create a surface
/// from it via raw-window-handle.
#[derive(Clone, Debug)]
pub(crate) struct RawWindow {
    native_window: android_activity::ndk::native_window::NativeWindow,
}

impl RawWindow {
    fn physical_size(&self) -> Size<DevicePixels> {
        Size {
            width: DevicePixels(self.native_window.width()),
            height: DevicePixels(self.native_window.height()),
        }
    }
}

impl raw_window_handle::HasWindowHandle for RawWindow {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let ptr =
            std::ptr::NonNull::new(self.native_window.ptr().as_ptr().cast::<std::ffi::c_void>())
                .ok_or(raw_window_handle::HandleError::Unavailable)?;
        let handle = raw_window_handle::AndroidNdkWindowHandle::new(ptr);
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(handle.into()) })
    }
}

impl raw_window_handle::HasDisplayHandle for RawWindow {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(raw_window_handle::DisplayHandle::android())
    }
}

#[derive(Default)]
pub(crate) struct AndroidWindowCallbacks {
    pub(crate) request_frame: Option<Box<dyn FnMut(RequestFrameOptions)>>,
    pub(crate) input: Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>,
    pub(crate) active_status_change: Option<Box<dyn FnMut(bool)>>,
    pub(crate) hover_status_change: Option<Box<dyn FnMut(bool)>>,
    pub(crate) resize: Option<Box<dyn FnMut(Size<Pixels>, f32)>>,
    pub(crate) moved: Option<Box<dyn FnMut()>>,
    pub(crate) should_close: Option<Box<dyn FnMut() -> bool>>,
    pub(crate) close: Option<Box<dyn FnOnce()>>,
    pub(crate) appearance_changed: Option<Box<dyn FnMut()>>,
    pub(crate) hit_test_window_control: Option<Box<dyn FnMut() -> Option<WindowControlArea>>>,
    pub(crate) insets_changed: Option<Box<dyn FnMut(WindowInsets)>>,
}

pub(crate) struct AndroidWindowState {
    pub(crate) renderer: WgpuRenderer,
    pub(crate) raw_window: RawWindow,
    pub(crate) bounds: Bounds<Pixels>,
    pub(crate) scale_factor: f32,
    pub(crate) title: String,
    pub(crate) input_handler: Option<PlatformInputHandler>,
    pub(crate) is_active: bool,
    pub(crate) mouse_position: Point<Pixels>,
    pub(crate) modifiers: Modifiers,
    pub(crate) capslock: Capslock,
    pub(crate) insets: WindowInsets,
    pub(crate) stable_insets: Option<Edges<Pixels>>,
}

pub(crate) struct AndroidWindowInner {
    pub(crate) app: AndroidApp,
    pub(crate) gpu_context: GpuContext,
    pub(crate) state: RefCell<AndroidWindowState>,
    pub(crate) callbacks: RefCell<AndroidWindowCallbacks>,
    pub(crate) click_state: RefCell<ClickState>,
    pub(crate) surface_configured: Cell<bool>,
    pub(crate) appearance: Cell<WindowAppearance>,
    soft_keyboard_requested: Cell<bool>,
    /// When the keyboard was last held across a focus change, if the hold is
    /// still live. See [`AndroidWindowInner::hold_soft_keyboard`].
    soft_keyboard_held_at: Cell<Option<Instant>>,
    /// The selection last handed to the Java-side IME mirror. The mirror
    /// computes the range of every committed edit from its own selection,
    /// so it has to be told when the GPUI cursor moves.
    last_ime_selection: RefCell<Option<Range<usize>>>,
    pending_physical_size: Cell<Option<Size<DevicePixels>>>,
}

pub struct AndroidWindow {
    pub(crate) inner: Rc<AndroidWindowInner>,
    display: Rc<dyn PlatformDisplay>,
    #[allow(dead_code)]
    handle: AnyWindowHandle,
}

fn surface_config(size: Size<DevicePixels>) -> WgpuSurfaceConfig {
    WgpuSurfaceConfig {
        size,
        transparent: false,
        // Mailbox avoids blocking in get_current_texture() during Android
        // lifecycle transitions; the renderer falls back to Fifo if unsupported.
        preferred_present_mode: Some(wgpu::PresentMode::Mailbox),
    }
}

pub(crate) fn scale_factor(app: &AndroidApp) -> f32 {
    app.config().density().map_or(2.0, |dpi| dpi as f32 / 160.0)
}

impl AndroidWindow {
    pub(crate) fn new(
        handle: AnyWindowHandle,
        _params: WindowParams,
        app: AndroidApp,
        gpu_context: GpuContext,
        display: Rc<dyn PlatformDisplay>,
        appearance: WindowAppearance,
    ) -> anyhow::Result<Self> {
        let native_window = app
            .native_window()
            .context("no native window: open_window must be called after the first InitWindow")?;
        let raw_window = RawWindow { native_window };
        let physical_size = raw_window.physical_size();
        let scale = scale_factor(&app);

        let renderer = WgpuRenderer::new(
            gpu_context.clone(),
            &raw_window,
            surface_config(physical_size),
            None,
        )?;

        let bounds = Bounds {
            origin: Point::default(),
            size: logical_size(physical_size, scale),
        };

        let state = AndroidWindowState {
            renderer,
            raw_window,
            bounds,
            scale_factor: scale,
            title: String::new(),
            input_handler: None,
            is_active: true,
            mouse_position: Point::default(),
            modifiers: Modifiers::default(),
            capslock: Capslock::default(),
            insets: WindowInsets::default(),
            stable_insets: None,
        };

        let inner = Rc::new(AndroidWindowInner {
            app,
            gpu_context,
            state: RefCell::new(state),
            callbacks: RefCell::new(AndroidWindowCallbacks::default()),
            click_state: RefCell::new(ClickState::default()),
            surface_configured: Cell::new(true),
            appearance: Cell::new(appearance),
            soft_keyboard_requested: Cell::new(false),
            soft_keyboard_held_at: Cell::new(None),
            last_ime_selection: RefCell::new(None),
            pending_physical_size: Cell::new(None),
        });

        inner.update_insets(true);

        Ok(Self {
            inner,
            display,
            handle,
        })
    }
}

fn logical_size(physical: Size<DevicePixels>, scale: f32) -> Size<Pixels> {
    Size {
        width: px(physical.width.0 as f32 / scale),
        height: px(physical.height.0 as f32 / scale),
    }
}

impl AndroidWindowInner {
    /// Called on `MainEvent::InitWindow` after the native window was destroyed
    /// and recreated (backgrounding, rotation). Recreates the wgpu surface on
    /// the same device so cached atlas textures stay valid.
    pub(crate) fn handle_surface_created(&self) {
        let Some(native_window) = self.app.native_window() else {
            log::error!("InitWindow received but native_window() returned None");
            return;
        };
        let raw_window = RawWindow { native_window };
        let physical_size = raw_window.physical_size();
        let Some(instance) = self
            .gpu_context
            .borrow()
            .as_ref()
            .map(|context| context.instance.clone())
        else {
            log::error!("surface recreation requested before the GPU context exists");
            return;
        };

        {
            let mut state = self.state.borrow_mut();
            if let Err(error) = state.renderer.replace_surface(
                &raw_window,
                surface_config(physical_size),
                &instance,
            ) {
                log::error!("failed to replace wgpu surface: {error:#}");
                return;
            }
            state.raw_window = raw_window;
        }
        self.surface_configured.set(true);
        self.update_size();
    }

    /// Called on `MainEvent::TerminateWindow`: the `ANativeWindow` is about to
    /// be destroyed, so rendering must stop until a new surface arrives.
    pub(crate) fn handle_surface_destroyed(&self) {
        self.surface_configured.set(false);
        self.reset_soft_keyboard_request();
        self.state.borrow_mut().renderer.unconfigure_surface();
    }

    pub(crate) fn update_size(&self) {
        let scale = scale_factor(&self.app);
        let (physical_size, changed, reset_insets) = {
            let mut state = self.state.borrow_mut();
            let physical_size = state.raw_window.physical_size();
            let logical = logical_size(physical_size, scale);
            let changed = state.bounds.size != logical || state.scale_factor != scale;
            // A keyboard-driven height change must retain the system-bar
            // baseline so it can be reported as IME coverage. Width or
            // density changes indicate rotation/configuration changes and
            // require learning a new baseline.
            let reset_insets = state.stable_insets.is_none()
                || state.scale_factor != scale
                || state.bounds.size.width != logical.width;
            state.bounds.size = logical;
            state.scale_factor = scale;
            (physical_size, changed, reset_insets)
        };

        // A rotation or density change changes the coordinate space in which
        // Android reports the content rectangle. Relearn the stable system
        // bars before separating the keyboard inset from it.
        self.update_insets(reset_insets);

        if !changed {
            return;
        }
        self.pending_physical_size.set(Some(physical_size));

        let logical = logical_size(physical_size, scale);
        let callback = self.callbacks.borrow_mut().resize.take();
        if let Some(mut callback) = callback {
            callback(logical, scale);
            let mut callbacks = self.callbacks.borrow_mut();
            if callbacks.resize.is_none() {
                callbacks.resize = Some(callback);
            }
        }
    }

    /// Converts Android's content rectangle into GPUI's logical inset model.
    /// NativeActivity exposes the visible content rectangle rather than typed
    /// `WindowInsets`; the first non-keyboard rectangle is therefore retained
    /// as the stable system-bar baseline and subsequent growth is reported as
    /// IME coverage.
    pub(crate) fn update_insets(&self, reset_stable: bool) {
        let (raw_insets, previous, stable) = {
            let state = self.state.borrow();
            let physical_size = state.raw_window.physical_size();
            let scale = state.scale_factor;
            let rect = self.app.content_rect();
            let width = physical_size.width.0.max(0);
            let height = physical_size.height.0.max(0);
            let clamp = |value: i32, limit: i32| value.max(0).min(limit) as f32 / scale;
            let raw_insets = Edges {
                left: px(clamp(rect.left, width)),
                top: px(clamp(rect.top, height)),
                right: px(clamp(width.saturating_sub(rect.right), width)),
                bottom: px(clamp(height.saturating_sub(rect.bottom), height)),
            };
            (raw_insets, state.insets.clone(), state.stable_insets)
        };

        let (next, callback) = {
            let mut state = self.state.borrow_mut();
            let mut stable = if reset_stable { None } else { stable };
            let stable_edges = stable.get_or_insert(raw_insets);

            // When the visible rectangle grows back to the stable area, the
            // keyboard has gone away. Allow Android to report a new baseline
            // (for example after an immersive-mode or rotation transition).
            if !reset_stable && raw_insets.bottom <= stable_edges.bottom {
                *stable_edges = raw_insets;
            }

            let ime = Edges {
                top: (raw_insets.top - stable_edges.top).max(px(0.)),
                right: (raw_insets.right - stable_edges.right).max(px(0.)),
                bottom: (raw_insets.bottom - stable_edges.bottom).max(px(0.)),
                left: (raw_insets.left - stable_edges.left).max(px(0.)),
            };
            let next = WindowInsets {
                safe_area: *stable_edges,
                ime,
            };
            state.stable_insets = Some(*stable_edges);
            if next == previous {
                return;
            }
            state.insets = next.clone();
            if next.ime == Edges::default() {
                self.soft_keyboard_requested.set(false);
            }
            (next, self.callbacks.borrow_mut().insets_changed.take())
        };

        if let Some(mut callback) = callback {
            callback(next);
            let mut callbacks = self.callbacks.borrow_mut();
            if callbacks.insets_changed.is_none() {
                callbacks.insets_changed = Some(callback);
            }
        }
    }

    pub(crate) fn show_soft_keyboard(&self) {
        if !self.soft_keyboard_requested.replace(true) {
            let (text, selection) = self.input_snapshot().unwrap_or_default();
            self.last_ime_selection.replace(Some(selection.clone()));
            if crate::ime::update_java_editor(&self.app, &text, selection, true) {
                return;
            }
            self.app.show_soft_input(true);
        }
    }

    pub(crate) fn hide_soft_keyboard(&self) {
        if !self.soft_keyboard_requested.get() {
            return;
        }
        // Android shows its selection toolbar without moving focus off the
        // editor, so the keyboard stays up behind it. GPUI's context menu is a
        // popup that does take focus, and the input losing focus reads as the
        // user leaving it. Hiding the keyboard there is doubly wrong: it
        // collapses a keyboard the user is still using, and the layout it
        // reflows out from under the menu was what the menu was positioned
        // against, leaving it floating away from the selection it belongs to.
        // A long press is the only gesture that opens such a menu, so the hold
        // is armed there and consumed by the focus loss it causes. It is
        // bounded in time because the menu takes focus on the spot: a hold
        // that outlived the gesture would swallow the next genuine focus loss
        // instead, and leave a keyboard up over a view the user had left.
        if self
            .soft_keyboard_held_at
            .take()
            .is_some_and(|held_at| held_at.elapsed() < CONTEXT_MENU_KEYBOARD_HOLD)
        {
            return;
        }
        self.soft_keyboard_requested.set(false);
        // The mirror is gone, so the next keyboard session starts fresh.
        self.last_ime_selection.replace(None);
        if !crate::ime::hide_java_editor(&self.app) {
            self.app.hide_soft_input(false);
        }
    }

    /// Keep the soft keyboard up across the next focus change.
    ///
    /// Armed by the long-press gesture, which is what opens a context menu.
    /// Dropping it again is [`AndroidWindowInner::release_soft_keyboard`]'s
    /// job: the hold only has to survive the one focus loss the menu causes,
    /// and any new finger-down means the user is driving the UI again.
    pub(crate) fn hold_soft_keyboard(&self) {
        self.soft_keyboard_held_at.set(Some(Instant::now()));
    }

    /// Drop a hold that no menu ended up consuming.
    pub(crate) fn release_soft_keyboard(&self) {
        self.soft_keyboard_held_at.set(None);
    }

    pub(crate) fn reset_soft_keyboard_request(&self) {
        self.soft_keyboard_requested.set(false);
    }

    /// The focused input's full text and its selection, in UTF-16 units.
    ///
    /// The length is deliberately not taken from `text_length_utf16`: that is a
    /// defaulted [`gpui::InputHandler`] method which returns `None` unless the
    /// input overrides it, and gpui-component's inputs do not, so asking for the
    /// length made this a silent no-op and left the IME mirror on a stale
    /// selection. An over-long range is safe instead — handlers clamp it and
    /// report the range they actually served through `actual_range`.
    fn input_snapshot(&self) -> Option<(String, Range<usize>)> {
        self.with_input_handler(|handler| {
            let selection = handler.selected_text_range(false)?.range;
            let mut actual_range = None;
            let text = handler.text_for_range(0..usize::MAX, &mut actual_range)?;
            Some((text, selection))
        })
        .flatten()
    }

    /// Push the current text and selection to the Java-side IME mirror.
    ///
    /// Called from the tap handler and from `set_input_handler`. The selection
    /// is checked first: reading the whole document and crossing into Java is
    /// only worth it when the mirror would see a change. Gating this on
    /// `soft_keyboard_requested` was wrong, since that flag tracks whether the
    /// keyboard was *asked for* and is cleared by inset changes while the IME
    /// is still up.
    ///
    /// GPUI only reports focus transitions through `text_input_state_changed`,
    /// so a cursor move would otherwise never reach the mirror. The mirror
    /// computes the range of each committed edit from its own selection, which
    /// is why text used to land at the position the mirror last saw rather than
    /// where the user had tapped.
    pub(crate) fn synchronize_soft_keyboard(&self) {
        let Some(selection) = self
            .with_input_handler(|handler| handler.selected_text_range(false))
            .flatten()
            .map(|selection| selection.range)
        else {
            return;
        };
        if self.last_ime_selection.borrow().as_ref() == Some(&selection) {
            return;
        }
        self.last_ime_selection.replace(Some(selection.clone()));
        if let Some((text, _)) = self.input_snapshot() {
            crate::ime::update_java_editor(&self.app, &text, selection, false);
        }
    }

    pub(crate) fn apply_pending_ime_events(&self) {
        let events = crate::ime::take_events();
        if events.is_empty() {
            return;
        }
        let mut events = Some(events);
        let applied = self.with_input_handler(|handler| {
            for event in events.take().expect("IME event queue already consumed") {
                match event {
                    ImeEvent::Replace { range, text } => {
                        handler.replace_text_in_range(Some(range), &text);
                    }
                    ImeEvent::SetSelection(range) => handler.set_selected_text_range(range),
                }
            }
        });
        if applied.is_none() {
            crate::ime::restore_events(events.expect("unapplied IME events missing"));
        }
    }

    pub(crate) fn request_frame(&self, force_render: bool) {
        if !self.surface_configured.get() {
            return;
        }
        let callback = self.callbacks.borrow_mut().request_frame.take();
        if let Some(mut callback) = callback {
            callback(RequestFrameOptions {
                require_presentation: true,
                force_render,
            });
            let mut callbacks = self.callbacks.borrow_mut();
            if callbacks.request_frame.is_none() {
                callbacks.request_frame = Some(callback);
            }
        }
    }

    pub(crate) fn set_active(&self, is_active: bool) {
        let changed = {
            let mut state = self.state.borrow_mut();
            let changed = state.is_active != is_active;
            state.is_active = is_active;
            changed
        };
        if !changed {
            return;
        }
        let callback = self.callbacks.borrow_mut().active_status_change.take();
        if let Some(mut callback) = callback {
            callback(is_active);
            let mut callbacks = self.callbacks.borrow_mut();
            if callbacks.active_status_change.is_none() {
                callbacks.active_status_change = Some(callback);
            }
        }
    }

    pub(crate) fn set_appearance(&self, appearance: WindowAppearance) {
        if self.appearance.replace(appearance) == appearance {
            return;
        }
        let callback = self.callbacks.borrow_mut().appearance_changed.take();
        if let Some(mut callback) = callback {
            callback();
            let mut callbacks = self.callbacks.borrow_mut();
            if callbacks.appearance_changed.is_none() {
                callbacks.appearance_changed = Some(callback);
            }
        }
    }

    pub(crate) fn dispatch_input(&self, input: PlatformInput) -> Option<DispatchEventResult> {
        let mut callback = self.callbacks.borrow_mut().input.take()?;
        let result = callback(input);
        let mut callbacks = self.callbacks.borrow_mut();
        if callbacks.input.is_none() {
            callbacks.input = Some(callback);
        }
        Some(result)
    }

    pub(crate) fn with_input_handler<R>(
        &self,
        f: impl FnOnce(&mut PlatformInputHandler) -> R,
    ) -> Option<R> {
        let mut handler = self.state.borrow_mut().input_handler.take()?;
        let result = f(&mut handler);
        let mut state = self.state.borrow_mut();
        if state.input_handler.is_none() {
            state.input_handler = Some(handler);
        }
        Some(result)
    }
}

impl raw_window_handle::HasWindowHandle for AndroidWindow {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let raw_window = self.inner.state.borrow().raw_window.clone();
        let handle = raw_window.window_handle()?.as_raw();
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(handle) })
    }
}

impl raw_window_handle::HasDisplayHandle for AndroidWindow {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(raw_window_handle::DisplayHandle::android())
    }
}

impl PlatformWindow for AndroidWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        self.inner.state.borrow().bounds
    }

    fn is_maximized(&self) -> bool {
        true
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Fullscreen(self.bounds())
    }

    fn content_size(&self) -> Size<Pixels> {
        self.inner.state.borrow().bounds.size
    }

    fn resize(&mut self, _size: Size<Pixels>) {}

    fn scale_factor(&self) -> f32 {
        self.inner.state.borrow().scale_factor
    }

    fn appearance(&self) -> WindowAppearance {
        self.inner.appearance.get()
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.display.clone())
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.inner.state.borrow().mouse_position
    }

    fn modifiers(&self) -> Modifiers {
        self.inner.state.borrow().modifiers
    }

    fn capslock(&self) -> Capslock {
        self.inner.state.borrow().capslock
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        // Deliberately does not summon the soft keyboard: GPUI re-registers
        // the handler every frame, so requesting it here re-opens a keyboard
        // the user just dismissed. Taps summon it instead (events.rs).
        self.inner.state.borrow_mut().input_handler = Some(input_handler);
        // Registering the handler is the only per-frame hook the platform gets,
        // and the IME mirror has to follow the cursor. Only a changed selection
        // crosses into Java.
        self.inner.synchronize_soft_keyboard();
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.inner.state.borrow_mut().input_handler.take()
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<futures::channel::oneshot::Receiver<usize>> {
        None
    }

    fn activate(&self) {
        self.inner.state.borrow_mut().is_active = true;
    }

    fn is_active(&self) -> bool {
        self.inner.state.borrow().is_active
    }

    fn is_hovered(&self) -> bool {
        false
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        WindowBackgroundAppearance::Opaque
    }

    fn set_title(&mut self, title: &str) {
        self.inner.state.borrow_mut().title = title.to_owned();
    }

    fn set_background_appearance(&self, _background: WindowBackgroundAppearance) {}

    fn minimize(&self) {}

    fn zoom(&self) {}

    fn toggle_fullscreen(&self) {}

    fn is_fullscreen(&self) -> bool {
        true
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.inner.callbacks.borrow_mut().request_frame = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        self.inner.callbacks.borrow_mut().input = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.inner.callbacks.borrow_mut().active_status_change = Some(callback);
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.inner.callbacks.borrow_mut().hover_status_change = Some(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.inner.callbacks.borrow_mut().resize = Some(callback);
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        self.inner.callbacks.borrow_mut().moved = Some(callback);
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.inner.callbacks.borrow_mut().should_close = Some(callback);
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.inner.callbacks.borrow_mut().close = Some(callback);
    }

    fn on_hit_test_window_control(&self, callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
        self.inner.callbacks.borrow_mut().hit_test_window_control = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.inner.callbacks.borrow_mut().appearance_changed = Some(callback);
    }

    fn draw(&self, scene: &Scene) {
        if !self.inner.surface_configured.get() {
            return;
        }
        let mut state = self.inner.state.borrow_mut();
        if let Some(physical_size) = self.inner.pending_physical_size.take() {
            state.renderer.update_drawable_size(physical_size);
        }
        state.renderer.draw(scene);
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.inner.state.borrow().renderer.sprite_atlas().clone()
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        self.inner
            .state
            .borrow()
            .renderer
            .supports_dual_source_blending()
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        Some(self.inner.state.borrow().renderer.gpu_specs())
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {}

    fn insets(&self) -> WindowInsets {
        self.inner.state.borrow().insets.clone()
    }

    fn on_insets_changed(&self, callback: Box<dyn FnMut(WindowInsets)>) {
        self.inner.callbacks.borrow_mut().insets_changed = Some(callback);
    }

    fn show_soft_keyboard(&self) {
        self.inner.show_soft_keyboard();
    }

    fn hide_soft_keyboard(&self) {
        self.inner.hide_soft_keyboard();
    }

    fn text_input_state_changed(&self, change: TextInputStateChange) {
        match change {
            TextInputStateChange::FocusGained => self.show_soft_keyboard(),
            TextInputStateChange::FocusLost => self.hide_soft_keyboard(),
            TextInputStateChange::SelectionChanged | TextInputStateChange::ContentChanged => {
                self.inner.synchronize_soft_keyboard();
            }
        }
    }

    fn request_decorations(&self, _decorations: WindowDecorations) {}

    fn show_window_menu(&self, _position: Point<Pixels>) {}

    fn start_window_move(&self) {}

    fn start_window_resize(&self, _edge: ResizeEdge) {}

    fn window_decorations(&self) -> Decorations {
        Decorations::Server
    }

    fn set_app_id(&mut self, _app_id: &str) {}

    fn window_controls(&self) -> WindowControls {
        WindowControls {
            fullscreen: false,
            maximize: false,
            minimize: false,
            window_menu: false,
        }
    }

    fn set_client_inset(&self, _inset: Pixels) {}
}
