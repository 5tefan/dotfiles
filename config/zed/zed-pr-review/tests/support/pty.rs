//! Unix PTY support shared by the native terminal regression drivers.
#![cfg(unix)]

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

pub const MOUSE_MODES: [u16; 5] = [1000, 1002, 1003, 1006, 1015];
pub const WAIT: Duration = Duration::from_secs(8);

pub struct Screen {
    parser: vte::Parser,
    state: ScreenState,
}

struct ScreenState {
    cells: Vec<Vec<String>>,
    rows: usize,
    columns: usize,
    x: usize,
    y: usize,
    modes: BTreeMap<u16, bool>,
    foreground: bool,
    colored_text: String,
    error: Option<String>,
}

impl Screen {
    pub fn new(rows: usize, columns: usize) -> Self {
        assert!(rows > 0 && columns > 0);
        Self {
            parser: vte::Parser::new(),
            state: ScreenState {
                cells: vec![vec![" ".into(); columns]; rows],
                rows,
                columns,
                x: 0,
                y: 0,
                modes: BTreeMap::new(),
                foreground: false,
                colored_text: String::new(),
                error: None,
            },
        }
    }

    pub fn feed(&mut self, data: &[u8]) -> Result<()> {
        self.parser.advance(&mut self.state, data);
        if let Some(error) = &self.state.error {
            bail!("{error}");
        }
        Ok(())
    }

    pub fn lines(&self) -> Vec<String> {
        self.state
            .cells
            .iter()
            .map(|row| row.concat().trim_end().to_owned())
            .collect()
    }

    pub fn text(&self) -> String {
        self.lines().join("\n")
    }

    pub fn body(&self) -> Vec<String> {
        self.lines()[1..self.state.rows - 2].to_vec()
    }

    pub fn viewport(&self) -> Option<(usize, usize, usize)> {
        let lines = self.lines();
        let text = lines[0].split_once("lines ")?.1;
        let (first, rest) = text.split_once('–')?;
        let (last, count) = rest.split_once('/')?;
        let count = count
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>();
        Some((first.parse().ok()?, last.parse().ok()?, count.parse().ok()?))
    }

    pub fn view(&self) -> Result<(usize, usize, usize)> {
        self.viewport()
            .context("No viewport in the terminal header")
    }

    pub fn current_visible(&self, change: &str) -> bool {
        let short = &change[..change.len().min(12)];
        self.body().iter().any(|line| {
            line.strip_prefix('@').is_some_and(|rest| {
                rest.starts_with(char::is_whitespace) && rest.trim_start().starts_with(short)
            })
        })
    }

    pub fn point(&self, text: &str) -> Result<(usize, usize)> {
        for (row, line) in self.body().iter().enumerate() {
            if let Some(column) = line.find(text) {
                // Mouse columns are display cells, not UTF-8 byte offsets.
                let column = unicode_width::UnicodeWidthStr::width(&line[..column]);
                return Ok((column + 1, row + 2));
            }
        }
        bail!("No visible {text:?}")
    }

    pub fn mode(&self, mode: u16) -> Option<bool> {
        self.state.modes.get(&mode).copied()
    }

    pub fn mouse_modes(&self) -> Vec<u16> {
        MOUSE_MODES
            .into_iter()
            .filter(|mode| self.mode(*mode) == Some(true))
            .collect()
    }

    pub fn assert_colored_graph(&self) -> Result<()> {
        ensure!(
            self.state.colored_text.contains("#17") || self.state.colored_text.contains("topic"),
            "The PR metadata had no foreground color"
        );
        Ok(())
    }
}

impl Perform for ScreenState {
    fn print(&mut self, character: char) {
        if self.foreground {
            self.colored_text.push(character);
        } else if !self.colored_text.ends_with('\n') {
            self.colored_text.push('\n');
        }
        let width = character.width().unwrap_or(0);
        if width == 0 {
            return;
        }
        if self.x >= self.columns {
            self.x = 0;
            self.y = (self.y + 1).min(self.rows - 1);
        }
        self.cells[self.y][self.x] = character.to_string();
        if width == 2 && self.x + 1 < self.columns {
            self.cells[self.y][self.x + 1].clear();
        }
        self.x += width;
    }

    fn execute(&mut self, byte: u8) {
        self.colored_text.push('\n');
        match byte {
            b'\r' => self.x = 0,
            b'\n' => self.y = (self.y + 1).min(self.rows - 1),
            8 => self.x = self.x.saturating_sub(1),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {
        self.colored_text.push('\n');
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        self.colored_text.push('\n');
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        // Metadata must occur within one colored text run, not be assembled
        // from characters separated by cursor moves or other escape sequences.
        self.colored_text.push('\n');
        let args: Vec<usize> = params.iter().map(|param| usize::from(param[0])).collect();
        let first = args.first().copied().unwrap_or(0);
        let n = first.max(1);
        if ignore || (!intermediates.is_empty() && intermediates != b"?") {
            self.error = Some(format!(
                "Unsupported terminal sequence: {intermediates:?} {args:?} {action:?}"
            ));
            return;
        }
        if intermediates == b"?" && matches!(action, 'h' | 'l') {
            for mode in args {
                self.modes.insert(mode as u16, action == 'h');
            }
            return;
        }
        match action {
            'H' | 'f' => {
                self.y = (n - 1).min(self.rows - 1);
                self.x = (args.get(1).copied().unwrap_or(0).max(1) - 1).min(self.columns - 1);
            }
            'A' => self.y = self.y.saturating_sub(n),
            'B' => self.y = (self.y + n).min(self.rows - 1),
            'C' => self.x = (self.x + n).min(self.columns - 1),
            'D' => self.x = self.x.saturating_sub(n),
            'G' => self.x = (n - 1).min(self.columns - 1),
            'd' => self.y = (n - 1).min(self.rows - 1),
            'J' => {
                for row in 0..self.rows {
                    for column in 0..self.columns {
                        if matches!(first, 2 | 3)
                            || (first == 0 && (row, column) >= (self.y, self.x))
                            || (first == 1 && (row, column) <= (self.y, self.x))
                        {
                            self.cells[row][column] = " ".into();
                        }
                    }
                }
            }
            'K' => {
                let (start, end) = match first {
                    0 => (self.x.min(self.columns), self.columns),
                    1 => (0, (self.x + 1).min(self.columns)),
                    2 => (0, self.columns),
                    _ => {
                        self.error = Some(format!("Unsupported line erase: {first}"));
                        return;
                    }
                };
                self.cells[self.y][start..end].fill(" ".into());
            }
            'm' => {
                // Skip extended-color payloads: palette/RGB numbers are not SGR codes.
                let mut index = 0;
                while index < args.len() {
                    match args[index] {
                        0 | 39 => self.foreground = false,
                        30..=37 | 90..=97 => self.foreground = true,
                        38 | 48 | 58 => {
                            if args[index] == 38 {
                                self.foreground = true;
                            }
                            index += match args.get(index + 1) {
                                Some(5) => 2,
                                Some(2) => 4,
                                _ => 0,
                            };
                        }
                        _ => {}
                    }
                    index += 1;
                }
            }
            'h' | 'l' => {}
            _ => {
                self.error = Some(format!(
                    "Unsupported terminal sequence: {args:?} {action:?}"
                ))
            }
        }
    }
}

/// The watchdog belongs to the driver, not to the viewer. All subprocesses stay
/// in this session's process group so the integration test can kill it as well.
pub fn session(timeout: Duration) -> Result<()> {
    // SAFETY: this is called once, at driver startup, before spawning threads.
    ensure!(
        unsafe { libc::setsid() } >= 0,
        "setsid: {}",
        io::Error::last_os_error()
    );
    // Closing the controlling terminal must not kill the supervising driver.
    unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };
    let group = std::process::id() as libc::pid_t;
    thread::spawn(move || {
        thread::sleep(timeout);
        eprintln!(
            "PTY scenario exceeded {} seconds; killing its process group",
            timeout.as_secs()
        );
        // SAFETY: the negative PID identifies the session created just above.
        unsafe { libc::kill(-group, libc::SIGKILL) };
    });
    Ok(())
}

pub fn attributes(fd: RawFd) -> Result<libc::termios> {
    let mut result = std::mem::MaybeUninit::uninit();
    // SAFETY: tcgetattr initializes result on success; fd is borrowed.
    ensure!(
        unsafe { libc::tcgetattr(fd, result.as_mut_ptr()) } == 0,
        "tcgetattr: {}",
        io::Error::last_os_error()
    );
    Ok(unsafe { result.assume_init() })
}

pub fn attributes_json(attributes: &libc::termios) -> Value {
    // Compare fields rather than struct padding. This is also the editor's
    // portable handoff format, matching the old termios attribute array.
    json!([
        attributes.c_iflag,
        attributes.c_oflag,
        attributes.c_cflag,
        attributes.c_lflag,
        unsafe { libc::cfgetispeed(attributes) },
        unsafe { libc::cfgetospeed(attributes) },
        attributes.c_cc.to_vec(),
    ])
}

pub fn terminal_attributes(fd: RawFd) -> Result<Value> {
    Ok(attributes_json(&attributes(fd)?))
}

pub struct Pty {
    pub master: File,
    pub slave: File,
    pub before: libc::termios,
    rows: usize,
    columns: usize,
}

impl Pty {
    pub fn open(rows: u16, columns: u16, controlling: bool) -> Result<Self> {
        let (mut master, mut slave) = (-1, -1);
        let mut size = libc::winsize {
            ws_row: rows,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: all pointers are valid; openpty returns two new owned descriptors.
        ensure!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut size,
                )
            } == 0,
            "openpty: {}",
            io::Error::last_os_error()
        );
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        for file in [&master, &slave] {
            // openpty does not set CLOEXEC. Only the explicit Stdio duplicates
            // should survive exec, never the driver's master or spare slave.
            ensure!(
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
                "fcntl(FD_CLOEXEC): {}",
                io::Error::last_os_error()
            );
        }
        let before = attributes(slave.as_raw_fd())?;
        let result = Self {
            master,
            slave,
            before,
            rows: usize::from(rows),
            columns: usize::from(columns),
        };
        let flags = unsafe { libc::fcntl(result.master.as_raw_fd(), libc::F_GETFL) };
        ensure!(
            flags >= 0
                && unsafe {
                    libc::fcntl(
                        result.master.as_raw_fd(),
                        libc::F_SETFL,
                        flags | libc::O_NONBLOCK,
                    )
                } == 0,
            "fcntl(O_NONBLOCK): {}",
            io::Error::last_os_error()
        );
        if controlling {
            // The driver is the session leader. Its children inherit this tty
            // and its foreground group, including access through /dev/tty.
            ensure!(
                unsafe { libc::ioctl(result.slave.as_raw_fd(), libc::TIOCSCTTY as _, 0) } == 0,
                "TIOCSCTTY: {}",
                io::Error::last_os_error()
            );
        }
        Ok(result)
    }

    pub fn assert_restored(&self) -> Result<()> {
        ensure!(
            terminal_attributes(self.slave.as_raw_fd())? == attributes_json(&self.before),
            "Raw mode was not restored"
        );
        Ok(())
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // SAFETY: the slave and saved attributes remain valid until fields drop.
        unsafe { libc::tcsetattr(self.slave.as_raw_fd(), libc::TCSANOW, &self.before) };
    }
}

pub fn readable(fd: RawFd, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let mut descriptor = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll borrows one initialized descriptor for the call.
        let result = unsafe {
            libc::poll(
                &mut descriptor,
                1,
                remaining.as_millis().min(i32::MAX as u128) as i32,
            )
        };
        if result >= 0 {
            ensure!(
                descriptor.revents & libc::POLLNVAL == 0,
                "poll: invalid descriptor {fd}"
            );
            return Ok(result > 0);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error.into());
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
    }
}

pub struct ManagedChild(pub Child);

impl ManagedChild {
    pub fn wait(&mut self, timeout: Duration) -> Result<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.0.try_wait()? {
                return Ok(status);
            }
            ensure!(
                Instant::now() < deadline,
                "Child {} did not exit within {timeout:?}",
                self.0.id()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            if let Err(error) = self.wait(Duration::from_secs(5)) {
                eprintln!("Failed to reap PTY child: {error:#}");
            }
        }
        // The outer integration watchdog always kills the driver's whole group,
        // including transports or editors orphaned by a failed assertion.
    }
}

pub struct Terminal {
    // Drop the child before restoring and closing the tty.
    pub child: ManagedChild,
    pub pty: Pty,
    pub screen: Screen,
    pub output: Vec<u8>,
}

impl Terminal {
    pub fn open(command: &mut Command, rows: u16, columns: u16, interactive: bool) -> Result<Self> {
        Self::spawn(command, Pty::open(rows, columns, interactive)?, interactive)
    }

    pub fn spawn(command: &mut Command, pty: Pty, interactive: bool) -> Result<Self> {
        if interactive {
            command.stdin(pty.slave.try_clone()?);
        } else {
            command.stdin(Stdio::piped());
        }
        command
            .stdout(pty.slave.try_clone()?)
            .stderr(pty.slave.try_clone()?);
        let mut child = ManagedChild(command.spawn().context("Start viewer on PTY")?);
        drop(child.0.stdin.take());
        let screen = Screen::new(pty.rows, pty.columns);
        Ok(Self {
            child,
            pty,
            screen,
            output: Vec::new(),
        })
    }

    pub fn transcript(&self) -> String {
        let start = self.output.len().saturating_sub(3000);
        format!(
            "Last terminal screen:\n{}\nLast terminal bytes: {:?}",
            self.screen.text(),
            String::from_utf8_lossy(&self.output[start..])
        )
    }

    pub fn pump(&mut self, timeout: Duration) -> Result<bool> {
        if !readable(self.pty.master.as_raw_fd(), timeout)? {
            return Ok(false);
        }
        let mut buffer = [0; 65536];
        let size = match self.pty.master.read(&mut buffer) {
            Ok(size) => size,
            Err(error)
                if error.raw_os_error() == Some(libc::EIO)
                    || matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error.into()),
        };
        self.output.extend_from_slice(&buffer[..size]);
        self.screen.feed(&buffer[..size])?;
        Ok(size > 0)
    }

    pub fn drain(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.pump(Duration::ZERO)? {
            ensure!(
                Instant::now() < deadline,
                "PTY output did not stop after exit"
            );
        }
        Ok(())
    }

    pub fn wait(
        &mut self,
        predicate: impl FnMut(&mut Self) -> Result<bool>,
        description: &str,
    ) -> Result<()> {
        self.wait_for(predicate, description, WAIT)
    }

    pub fn wait_for(
        &mut self,
        mut predicate: impl FnMut(&mut Self) -> Result<bool>,
        description: &str,
        timeout: Duration,
    ) -> Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.pump(Duration::from_millis(50))?;
            if predicate(self)? {
                return Ok(());
            }
            ensure!(
                self.child.0.try_wait()?.is_none(),
                "Viewer exited while waiting for {description}"
            );
        }
        bail!("Timed out waiting for {description}")
    }

    pub fn keep(
        &mut self,
        mut predicate: impl FnMut(&mut Self) -> Result<bool>,
        duration: Duration,
        description: &str,
    ) -> Result<()> {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            self.pump(Duration::from_millis(50))?;
            ensure!(
                self.child.0.try_wait()?.is_none(),
                "Viewer exited unexpectedly"
            );
            ensure!(predicate(self)?, "{description}");
        }
        Ok(())
    }

    pub fn send(&mut self, mut bytes: &[u8]) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !bytes.is_empty() {
            match self.pty.master.write(bytes) {
                Ok(0) => bail!("PTY input closed"),
                Ok(count) => bytes = &bytes[count..],
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    ensure!(Instant::now() < deadline, "PTY input blocked");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub fn mouse(&mut self, button: u8, point: (usize, usize), release: bool) -> Result<()> {
        self.send(
            format!(
                "\x1b[<{button};{};{}{}",
                point.0,
                point.1,
                if release { 'm' } else { 'M' }
            )
            .as_bytes(),
        )
    }

    pub fn click(&mut self, point: (usize, usize)) -> Result<()> {
        self.mouse(0, point, false)?;
        self.mouse(0, point, true)
    }

    pub fn assert_active(&self) -> Result<()> {
        ensure!(
            self.screen.mode(1049) == Some(true),
            "Alternate screen was not enabled"
        );
        ensure!(
            self.screen.mode(1006) == Some(true),
            "SGR mouse reporting was not enabled"
        );
        let flags = attributes(self.pty.slave.as_raw_fd())?.c_lflag;
        ensure!(
            flags & (libc::ICANON | libc::ECHO | libc::ISIG) == 0,
            "Terminal is not raw"
        );
        Ok(())
    }

    pub fn assert_restored(&self, mouse_modes: &[u16]) -> Result<()> {
        self.pty.assert_restored()?;
        ensure!(
            self.screen.mode(1049) == Some(false),
            "Alternate screen was not left"
        );
        ensure!(
            !mouse_modes.is_empty()
                && mouse_modes
                    .iter()
                    .all(|mode| self.screen.mode(*mode) == Some(false)),
            "Mouse capture was not disabled"
        );
        ensure!(
            self.screen.mode(25) == Some(true),
            "Cursor was not restored"
        );
        Ok(())
    }

    pub fn exit_and_assert_restored(&mut self, key: &[u8]) -> Result<()> {
        let mouse_modes = self.screen.mouse_modes();
        self.send(key)?;
        self.wait(
            |terminal| Ok(terminal.child.0.try_wait()?.is_some()),
            "clean exit",
        )?;
        self.drain()?;
        let status = self.child.0.try_wait()?.context("Viewer has not exited")?;
        ensure!(status.success(), "Exit status: {status}");
        self.assert_restored(&mouse_modes)
    }
}

/// Files avoid pipe-buffer deadlocks while preserving bounded process waits.
pub fn output(command: &mut Command, timeout: Duration) -> Result<Output> {
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?);
    let mut child = ManagedChild(
        command
            .spawn()
            .with_context(|| format!("Spawn {command:?}"))?,
    );
    let status = child
        .wait(timeout)
        .with_context(|| format!("Run {command:?}"))?;
    use std::io::{Seek, SeekFrom};
    stdout.seek(SeekFrom::Start(0))?;
    stderr.seek(SeekFrom::Start(0))?;
    let (mut out, mut err) = (Vec::new(), Vec::new());
    stdout.read_to_end(&mut out)?;
    stderr.read_to_end(&mut err)?;
    Ok(Output {
        status,
        stdout: out,
        stderr: err,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incremental_utf8_cursor_erase_modes_and_osc() -> Result<()> {
        let bytes = b"\x1b]0;ignored title\x07\x1b]2;another\x1b\\\x1b[2J\x1b[?1049;1006h\x1b[?25l\x1b[2;3H\xe7\x95\x8c\xe2\x97\x8b\x1b[1D!\x1b[3;1Habc\x1b[2D\x1b[K\x1b[4dxyz\rQ\nR\x08S";
        for chunk in 1..=bytes.len() {
            let mut screen = Screen::new(7, 12);
            for part in bytes.chunks(chunk) {
                screen.feed(part)?;
            }
            assert_eq!(screen.lines(), ["", "  界!", "a", "Qxyz", " S", "", ""]);
            assert_eq!(screen.mode(1049), Some(true));
            assert_eq!(screen.mode(1006), Some(true));
            assert_eq!(screen.mode(25), Some(false));
            screen.feed(
                b"\x1b[2;3H\x1b[1J\x1b[4;2H\x1b[1K\x1b[5;1H\x1b[2K\x1b[?1049;1006l\x1b[?25h",
            )?;
            assert_eq!(screen.lines()[3], "  yz");
            assert_eq!(screen.lines()[4], "");
            assert_eq!(screen.mode(1049), Some(false));
            assert_eq!(screen.mode(1006), Some(false));
            assert_eq!(screen.mode(25), Some(true));
        }
        Ok(())
    }

    #[test]
    fn viewport_wide_mouse_points_and_combining_characters() -> Result<()> {
        let mut screen = Screen::new(6, 80);
        screen.feed("PR stack · lines 4–6/25\r\n@ abcdefghijkl 界e\u{301} target".as_bytes())?;
        assert_eq!(screen.viewport(), Some((4, 6, 25)));
        assert!(screen.current_visible("abcdefghijklmnop"));
        assert_eq!(screen.point("target")?, (20, 2));
        screen.feed(b"\x1b[2A\x1b[80C\x1b[1G\x1b[3B\x1b[1;1f\x1b[0J")?;
        assert!(screen.lines().iter().all(String::is_empty));
        Ok(())
    }

    #[test]
    fn graph_color_requires_colored_metadata_not_just_header() -> Result<()> {
        let mut screen = Screen::new(6, 80);
        screen.feed(b"\x1b[31mPR stack\x1b[39m\r\n@ #17 topic")?;
        assert!(screen.assert_colored_graph().is_err());
        screen.feed(b"\x1b[31m#\x1b[3C17\x1b[0m\x1b[48;5;31m topic\x1b[0m")?;
        assert!(screen.assert_colored_graph().is_err());
        screen.feed(b"\x1b[38;2;0;39;90m#17\x1b[0m")?;
        screen.assert_colored_graph()?;
        assert!(screen.feed(b"\x1b[1X").is_err());
        Ok(())
    }
}
