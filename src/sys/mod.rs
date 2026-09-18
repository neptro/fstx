//! Platform backends implementing [`crate::vfs::Vfs`].

#[cfg(target_os = "linux")]
pub(crate) mod linux;
