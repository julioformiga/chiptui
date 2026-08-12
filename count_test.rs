fn count_serial_ports() -> Option<usize> {
    #[cfg(unix)]
    {
        let mut count = 0;
        if let Ok(entries) = std::fs::read_dir("/dev") {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with("ttyUSB") || name.starts_with("ttyACM") || name.starts_with("cu.usb") || name.starts_with("tty.usb") {
                        count += 1;
                    }
                }
            }
        }
        Some(count)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

fn main() {
    println!("{:?}", count_serial_ports());
}
