use android_activity::AndroidApp;
use jni::{
    EnvUnowned, JValue, JavaVM,
    objects::{JClass, JObject, JString},
    refs::Global,
    sys::jint,
};
use parking_lot::Mutex;
use std::{collections::VecDeque, ops::Range, sync::OnceLock};

#[derive(Debug)]
pub(crate) enum ImeEvent {
    Replace { range: Range<usize>, text: String },
    SetSelection(Range<usize>),
}

fn pending_events() -> &'static Mutex<VecDeque<ImeEvent>> {
    static EVENTS: OnceLock<Mutex<VecDeque<ImeEvent>>> = OnceLock::new();
    EVENTS.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn push_event(event: ImeEvent) {
    pending_events().lock().push_back(event);
}

pub(crate) fn take_events() -> VecDeque<ImeEvent> {
    std::mem::take(&mut *pending_events().lock())
}

pub(crate) fn restore_events(mut events: VecDeque<ImeEvent>) {
    let mut pending = pending_events().lock();
    events.append(&mut pending);
    *pending = events;
}

pub(crate) fn clear_events() {
    pending_events().lock().clear();
}

pub(crate) fn update_java_editor(
    app: &AndroidApp,
    text: &str,
    selection: Range<usize>,
    show: bool,
) -> bool {
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let result = vm.attach_current_thread(|env| -> jni::errors::Result<()> {
        let raw_activity = app.activity_as_ptr() as jni::sys::jobject;
        let activity = unsafe { env.as_cast_raw::<Global<JObject>>(&raw_activity)? };
        let text = JString::from_str(env, text)?;
        let method = if show {
            jni::jni_str!("showGpuiKeyboard")
        } else {
            jni::jni_str!("syncGpuiText")
        };
        let result = env.call_method(
            activity,
            method,
            jni::jni_sig!((text: JString, start: i32, end: i32) -> ()),
            &[
                JValue::Object(text.as_ref()),
                JValue::Int(selection.start.min(i32::MAX as usize) as i32),
                JValue::Int(selection.end.min(i32::MAX as usize) as i32),
            ],
        );
        if result.is_err() {
            let _ = env.exception_clear();
        }
        result.map(|_| ())
    });
    if let Err(error) = result {
        log::warn!("failed to synchronize Android IME editor: {error:?}");
        false
    } else {
        true
    }
}

pub(crate) fn hide_java_editor(app: &AndroidApp) -> bool {
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let result = vm.attach_current_thread(|env| -> jni::errors::Result<()> {
        let raw_activity = app.activity_as_ptr() as jni::sys::jobject;
        let activity = unsafe { env.as_cast_raw::<Global<JObject>>(&raw_activity)? };
        let result = env.call_method(
            activity,
            jni::jni_str!("hideGpuiKeyboard"),
            jni::jni_sig!(() -> ()),
            &[],
        );
        if result.is_err() {
            let _ = env.exception_clear();
        }
        result.map(|_| ())
    });
    if let Err(error) = result {
        log::warn!("failed to hide Android IME editor: {error:?}");
        false
    } else {
        true
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_vibex_mobile_GpuiNativeActivity_nativeReplaceText<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    start: jint,
    before: jint,
    replacement: JString<'caller>,
) {
    unowned_env
        .with_env(|_| -> jni::errors::Result<()> {
            let start = start.max(0) as usize;
            let before = before.max(0) as usize;
            push_event(ImeEvent::Replace {
                range: start..start.saturating_add(before),
                text: replacement.to_string(),
            });
            Ok(())
        })
        .resolve::<jni::errors::LogErrorAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_ai_vibex_mobile_GpuiNativeActivity_nativeSetSelection<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    start: jint,
    end: jint,
) {
    unowned_env
        .with_env(|_| -> jni::errors::Result<()> {
            let start = start.max(0) as usize;
            let end = end.max(0) as usize;
            push_event(ImeEvent::SetSelection(start.min(end)..start.max(end)));
            Ok(())
        })
        .resolve::<jni::errors::LogErrorAndDefault>()
}
