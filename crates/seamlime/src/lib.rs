//! Seamlime application library.

pub mod audio;

/// Starts the application. Task 12 replaces this with the real window.
pub fn run() -> eframe::Result<()> {
    println!("{}", seamlime_core::software_tag());
    Ok(())
}
