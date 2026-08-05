use std::fs::File;

use fs2::FileExt;

pub(super) fn try_lock_exclusive(file: &File) -> crate::Result<bool> {
    match FileExt::try_lock_exclusive(file) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn try_lock_shared(file: &File) -> crate::Result<bool> {
    match FileExt::try_lock_shared(file) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(error.into()),
    }
}
