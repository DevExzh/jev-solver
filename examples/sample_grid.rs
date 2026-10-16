//! Generate a deterministic visual smoke-test input; red squares are 1, 5, 9.
use std::{fs, path::PathBuf};

use anyhow::Result;
use image::{Rgb, RgbImage};

fn main() -> Result<()> {
    let path = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "artifacts/sample-grid.png".to_owned()),
    );
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let mut image = RgbImage::from_pixel(300, 300, Rgb([245, 245, 245]));
    for index in 0..9 {
        let left = (index % 3) * 100;
        let top = (index / 3) * 100;
        let red_square = [0, 4, 8].contains(&index);
        for y in 20..80 {
            for x in 20..80 {
                if red_square || (x as i32 - 50).pow(2) + (y as i32 - 50).pow(2) < 30 * 30 {
                    image.put_pixel(
                        left + x,
                        top + y,
                        if red_square {
                            Rgb([220, 25, 25])
                        } else {
                            Rgb([25, 65, 220])
                        },
                    );
                }
            }
        }
    }
    image.save(&path)?;
    println!("{} (red squares: tiles 1, 5, 9)", path.display());
    Ok(())
}
