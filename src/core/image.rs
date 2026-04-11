//! Utilities for passing image data to CLI agents via temp-file path injection.
//!
//! CLI agents (Claude Code, Codex, etc.) do not accept raw binary image data on
//! stdin. The standard workaround is to write the image to a temp file and
//! reference its absolute path in the prompt. The agent's `Read` tool then
//! picks up the file automatically.

use std::path::PathBuf;

/// Write `image_bytes` to a temporary file named `filename` and return both
/// the absolute path and a ready-to-use prompt string referencing the file.
///
/// The caller is responsible for deleting the file when it is no longer needed.
///
/// # Platform notes
///
/// On Windows the temp directory is resolved via `%USERPROFILE%\AppData\Local\Temp`
/// to avoid 8.3 short-name paths that some tools reject. On all other platforms
/// [`std::env::temp_dir()`] is used.
///
/// # Example
///
/// ```no_run
/// use gate4agent::image_to_prompt_reference;
///
/// let bytes = std::fs::read("screenshot.png").unwrap();
/// let (path, prompt) = image_to_prompt_reference(&bytes, "screenshot.png").unwrap();
/// // `prompt` is now "Analyze this image: C:\Users\…\AppData\Local\Temp\screenshot.png"
/// // Pass `prompt` as the initial prompt to a PipeSession / AcpSession.
/// ```
pub fn image_to_prompt_reference(
    image_bytes: &[u8],
    filename: &str,
) -> std::io::Result<(PathBuf, String)> {
    let dir = resolve_temp_dir();
    let path = dir.join(filename);
    std::fs::write(&path, image_bytes)?;
    let prompt = format!("Analyze this image: {}", path.display());
    Ok((path, prompt))
}

/// Resolve the best available temp directory for this platform.
fn resolve_temp_dir() -> PathBuf {
    if cfg!(windows) {
        // Prefer the long-path-safe Windows temp directory.
        std::env::var("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir())
            .join("AppData")
            .join("Local")
            .join("Temp")
    } else {
        std::env::temp_dir()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_to_prompt_reference_writes_file_and_returns_path() {
        let bytes = b"fake image data";
        let filename = "gate4agent_test_image.bin";
        let (path, prompt) = image_to_prompt_reference(bytes, filename)
            .expect("should write temp file");

        assert!(path.exists(), "temp file must exist after write");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(
            prompt.contains(&path.display().to_string()),
            "prompt must contain the absolute path"
        );

        // Clean up.
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn image_to_prompt_reference_prefix() {
        let bytes = b"x";
        let (path, prompt) = image_to_prompt_reference(bytes, "gate4agent_prefix_test.bin")
            .unwrap();
        assert!(
            prompt.starts_with("Analyze this image: "),
            "prompt must start with the standard prefix, got: {prompt}"
        );
        let _ = std::fs::remove_file(&path);
    }
}
