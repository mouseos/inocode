//! Reading a secret typed at the terminal without showing it.

use std::io::{self, BufRead};

/// One line from standard input, without its line ending. When standard
/// input is a terminal, what is typed is not echoed; piped input is read as
/// it comes.
pub fn read_line_hidden() -> io::Result<String> {
    let echo = Echo::off();
    let mut line = String::new();
    let read = io::stdin().lock().read_line(&mut line);
    drop(echo);
    read?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Turns the terminal's echo off while it lives.
struct Echo {
    #[cfg(windows)]
    restore: Option<(windows_sys::Win32::Foundation::HANDLE, u32)>,
    #[cfg(target_os = "linux")]
    restore: Option<libc::termios>,
    #[cfg(all(unix, not(target_os = "linux")))]
    restore: bool,
}

impl Echo {
    #[cfg(windows)]
    fn off() -> Self {
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_ECHO_INPUT, STD_INPUT_HANDLE,
        };
        // SAFETY: the handle is the process's own standard input, and the
        // mode is read before it is changed.
        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            let mut mode = 0u32;
            if GetConsoleMode(handle, &mut mode) == 0 {
                return Self { restore: None };
            }
            if SetConsoleMode(handle, mode & !ENABLE_ECHO_INPUT) == 0 {
                return Self { restore: None };
            }
            Self {
                restore: Some((handle, mode)),
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn off() -> Self {
        // SAFETY: `termios` is plain data filled by `tcgetattr` before use.
        unsafe {
            let mut settings: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut settings) != 0 {
                return Self { restore: None };
            }
            let mut hidden = settings;
            hidden.c_lflag &= !libc::ECHO;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &hidden) != 0 {
                return Self { restore: None };
            }
            Self {
                restore: Some(settings),
            }
        }
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    fn off() -> Self {
        use std::io::IsTerminal;
        let hidden = io::stdin().is_terminal()
            && std::process::Command::new("stty")
                .arg("-echo")
                .stdin(std::process::Stdio::inherit())
                .status()
                .is_ok_and(|status| status.success());
        Self { restore: hidden }
    }
}

impl Drop for Echo {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some((handle, mode)) = self.restore {
            // SAFETY: restores the mode read from the same handle.
            unsafe {
                windows_sys::Win32::System::Console::SetConsoleMode(handle, mode);
            }
            println!();
        }
        #[cfg(target_os = "linux")]
        if let Some(settings) = self.restore {
            // SAFETY: restores the settings read from the same descriptor.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &settings);
            }
            println!();
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        if self.restore {
            let _ = std::process::Command::new("stty")
                .arg("echo")
                .stdin(std::process::Stdio::inherit())
                .status();
            println!();
        }
    }
}
