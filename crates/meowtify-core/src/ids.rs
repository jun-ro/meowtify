//! The id a download carries in its file name, and the cover address that goes with a YouTube id.
//!
//! Downloads are named `Title [id].ext`. Several crates need the id back from a path, so the
//! rule lives here once. Only the file name is read: a bracket in a folder name is not an id.

/// The id in a download's file name, whichever provider it came from. Any non-empty text between
/// the last `[` and the `].` before the extension counts.
pub fn download_id(path: &str) -> Option<&str> {
    let name = path.rsplit(['/', '\\']).next()?;
    let end = name.rfind("].")?;
    let start = name[..end].rfind('[')? + 1;
    let id = &name[start..end];
    (!id.is_empty()).then_some(id)
}

/// The YouTube video id in a download's file name: a [`download_id`] of exactly 11 characters
/// from `A-Z a-z 0-9 - _`. Other providers' ids do not pass, so they never reach a YouTube address.
pub fn youtube_id(path: &str) -> Option<&str> {
    let id = download_id(path)?;
    (id.len() == 11 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')).then_some(id)
}

/// The address of a YouTube video's cover image. `video_id` must be a valid id.
pub fn youtube_thumbnail(video_id: &str) -> String {
    format!("https://i.ytimg.com/vi/{video_id}/mqdefault.jpg")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_id_from_the_file_name_only() {
        assert_eq!(download_id(r"C:\Music [x]\Song [abc123XYZ_-].m4a"), Some("abc123XYZ_-"));
        assert_eq!(download_id("/m/A - B [abc-_DEF123].opus"), Some("abc-_DEF123"));
        assert_eq!(download_id("/m/Song [live] remix.m4a"), None);
        assert_eq!(download_id("/m/plain.mp3"), None);
        assert_eq!(download_id("/m/Song [].mp3"), None, "an empty id is no id");
        assert_eq!(download_id("/m/Song [dQw4w9WgXcQ] extra.mp3"), None);
        assert_eq!(download_id(r"C:\Music [dQw4w9WgXcQ].d\plain.mp3"), None, "a bracket in a folder name is not an id");
        assert_eq!(download_id("/m/Song [a].tar.gz"), Some("a"), "the extension may have a dot");
    }

    #[test]
    fn youtube_ids_are_eleven_safe_characters() {
        assert_eq!(youtube_id(r"C:\Music\laced up [NWrZ__uGAf4].m4a"), Some("NWrZ__uGAf4"));
        assert_eq!(youtube_id("/m/Song [short].mp3"), None);
        assert_eq!(youtube_id("/m/Song [12345678901/].mp3"), None, "no path characters");
        assert_eq!(youtube_id("/m/Song [12345678\u{e9}1].mp3"), None, "ASCII only");
        assert_eq!(download_id("/m/Song [short].mp3"), Some("short"), "other providers' ids still count as downloads");
        assert_eq!(youtube_thumbnail("NWrZ__uGAf4"), "https://i.ytimg.com/vi/NWrZ__uGAf4/mqdefault.jpg");
    }
}
