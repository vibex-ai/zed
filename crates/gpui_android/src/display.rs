use anyhow::Result;
use gpui::{Bounds, DisplayId, Pixels, PlatformDisplay, Point, Size, px};

#[derive(Debug)]
pub struct AndroidDisplay {
    id: DisplayId,
    uuid: uuid::Uuid,
    size: parking_lot::RwLock<Size<Pixels>>,
}

impl AndroidDisplay {
    pub fn new() -> Self {
        AndroidDisplay {
            id: DisplayId::new(1),
            uuid: uuid::Uuid::new_v4(),
            size: parking_lot::RwLock::new(Size {
                width: px(412.),
                height: px(915.),
            }),
        }
    }

    pub(crate) fn set_size(&self, size: Size<Pixels>) {
        *self.size.write() = size;
    }
}

impl PlatformDisplay for AndroidDisplay {
    fn id(&self) -> DisplayId {
        self.id
    }

    fn uuid(&self) -> Result<uuid::Uuid> {
        Ok(self.uuid)
    }

    fn bounds(&self) -> Bounds<Pixels> {
        Bounds {
            origin: Point::default(),
            size: *self.size.read(),
        }
    }
}
