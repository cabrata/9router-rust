//! Terminal UI helpers: spinners, menus, and display utilities.

use anyhow::Result;
use std::io::{self, Write};

/// A simple terminal spinner with animation.
pub struct Spinner {
    text: String,
    frames: Vec<&'static str>,
    current_frame: usize,
}

impl Spinner {
    const FRAMES: &'static [&'static str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

    /// Create a new spinner with the given text.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            frames: Self::FRAMES.to_vec(),
            current_frame: 0,
        }
    }

    /// Start the spinner (displays immediately).
    pub fn start(&mut self) -> Result<()> {
        self.render()?;
        Ok(())
    }

    /// Update the spinner frame.
    pub fn tick(&mut self) -> Result<()> {
        self.current_frame = (self.current_frame + 1) % self.frames.len();
        self.render()?;
        Ok(())
    }

    /// Render the current frame.
    fn render(&self) -> Result<()> {
        let frame = self.frames[self.current_frame];
        print!("\r{} {}", frame, self.text);
        io::stdout().flush()?;
        Ok(())
    }

    /// Clear the spinner line.
    pub fn stop(&self) -> Result<()> {
        print!("\r\x1b[K");
        io::stdout().flush()?;
        Ok(())
    }

    /// Stop and print success message.
    pub fn succeed(&self, msg: impl Into<String>) -> Result<()> {
        self.stop()?;
        println!("✅ {}", msg.into());
        Ok(())
    }

    /// Stop and print failure message.
    pub fn fail(&self, msg: impl Into<String>) -> Result<()> {
        self.stop()?;
        println!("❌ {}", msg.into());
        Ok(())
    }
}

/// Clear the terminal screen.
pub fn clear_screen() -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        use std::process::Command;
        Command::new("cmd")
            .args(&["/C", "cls"])
            .status()?;
    }

    #[cfg(not(target_os = "windows"))]
    {
        print!("\x1b[2J\x1b[H");
        io::stdout().flush()?;
    }

    Ok(())
}

/// Print a colored warning message.
pub fn warn(msg: impl Into<String>) {
    println!("\x1b[33m⚠️  {}\x1b[0m", msg.into());
}

/// Print a colored info message.
pub fn info(msg: impl Into<String>) {
    println!("\x1b[36mℹ️  {}\x1b[0m", msg.into());
}

/// Print a colored success message.
pub fn success(msg: impl Into<String>) {
    println!("\x1b[32m✅ {}\x1b[0m", msg.into());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spinner_creation() {
        let spinner = Spinner::new("test");
        assert_eq!(spinner.text, "test");
        assert_eq!(spinner.current_frame, 0);
    }

    #[test]
    fn test_spinner_frame_increment() {
        let mut spinner = Spinner::new("test");
        spinner.tick().ok();
        assert_eq!(spinner.current_frame, 1);
        spinner.tick().ok();
        assert_eq!(spinner.current_frame, 2);
    }

    #[test]
    fn test_spinner_frame_wrap() {
        let mut spinner = Spinner::new("test");
        spinner.current_frame = Spinner::FRAMES.len() - 1;
        spinner.tick().ok();
        assert_eq!(spinner.current_frame, 0); // Should wrap around
    }
}
