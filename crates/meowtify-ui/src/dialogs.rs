//! Native file dialogs. iOS has none (`rfd` does not support it), so each function returns
//! `None` there and the caller treats it as a cancelled dialog.

use std::path::PathBuf;

#[cfg(not(target_os = "ios"))]
pub(crate) fn pick_folder(title: &str, start: Option<PathBuf>) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().set_title(title);
    if let Some(dir) = start {
        dialog = dialog.set_directory(dir);
    }
    dialog.pick_folder()
}

#[cfg(not(target_os = "ios"))]
pub(crate) fn save_file(title: &str, name: &str, filter: (&str, &[&str]), start: Option<PathBuf>) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().set_title(title).set_file_name(name).add_filter(filter.0, filter.1);
    if let Some(dir) = start {
        dialog = dialog.set_directory(dir);
    }
    dialog.save_file()
}

#[cfg(not(target_os = "ios"))]
pub(crate) fn pick_file(title: &str, filter: (&str, &[&str])) -> Option<PathBuf> {
    rfd::FileDialog::new().set_title(title).add_filter(filter.0, filter.1).pick_file()
}

#[cfg(target_os = "ios")]
pub(crate) fn pick_folder(_title: &str, _start: Option<PathBuf>) -> Option<PathBuf> {
    None
}

#[cfg(target_os = "ios")]
pub(crate) fn save_file(_title: &str, _name: &str, _filter: (&str, &[&str]), _start: Option<PathBuf>) -> Option<PathBuf> {
    None
}

#[cfg(target_os = "ios")]
pub(crate) fn pick_file(_title: &str, _filter: (&str, &[&str])) -> Option<PathBuf> {
    None
}
