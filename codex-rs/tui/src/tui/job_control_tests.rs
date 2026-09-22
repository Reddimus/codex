use std::fs::File;
use std::io::Read;
use std::mem::MaybeUninit;
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use pretty_assertions::assert_eq;

const DRIVER: &str = "tui::job_control::tests::job_control_driver";
const JOB: &str = "tui::job_control::tests::job_control_worker";
const CYCLES: usize = 100;

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn fixture(name: &str, directory: &Path) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("test executable"));
    command
        .args(["--exact", name, "--ignored", "--nocapture"])
        .env("CODEX_JOB_CONTROL_TEST_DIR", directory)
        .env("TERM", "xterm-256color")
        .env_remove("CODEX_TUI_DISABLE_KEYBOARD_ENHANCEMENT");
    command
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "waiting for {path:?}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn suspension_preserves_shell_modes_until_foreground_resume() {
    let mut master = -1;
    let mut slave = -1;
    // SAFETY: openpty initializes the descriptors; optional parameters are null.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    // SAFETY: openpty returned two independently owned descriptors.
    let master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    let directory = tempfile::tempdir().expect("fixture directory");
    let mut command = fixture(DRIVER, directory.path());
    command
        .stdin(Stdio::from(slave.try_clone().expect("clone slave")))
        .stdout(Stdio::from(slave))
        .stderr(Stdio::piped());
    // SAFETY: only async-signal-safe syscalls run between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut driver = OwnedChild(command.spawn().expect("spawn session leader"));
    drop(command);
    // Drain the PTY without retaining unbounded output or blocking fixture writes.
    let reader = std::thread::spawn(move || {
        let mut master = master;
        let mut buffer = [0; 4096];
        while let Ok(count) = master.read(&mut buffer) {
            if count == 0 {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = driver.0.try_wait().expect("poll driver") {
            break status;
        }
        if Instant::now() >= deadline {
            // The driver retains its unreaped job until exit, so its pid cannot
            // be reused while the session leader is still alive.
            if let Ok(pid) = std::fs::read_to_string(directory.path().join("job-pid"))
                && let Ok(pid) = pid.parse::<libc::pid_t>()
            {
                // SAFETY: this is the isolated fixture's retained child group.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
            panic!("job-control fixture timed out");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    reader.join().expect("PTY reader");
    let mut errors = String::new();
    driver
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut errors)
        .unwrap();
    assert!(status.success(), "job-control fixture failed: {errors}");
}

fn wait_stopped(pid: libc::pid_t) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut status = 0;
        // SAFETY: pid is an owned, unreaped child and status is writable.
        let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG | libc::WUNTRACED) };
        assert!(result >= 0, "waitpid: {}", std::io::Error::last_os_error());
        if result == pid {
            assert!(
                libc::WIFSTOPPED(status),
                "job exited before stopping: {status}"
            );
            return;
        }
        assert!(Instant::now() < deadline, "job did not stop");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn termios() -> libc::termios {
    let mut termios = MaybeUninit::uninit();
    // SAFETY: tcgetattr initializes the provided termios on success.
    assert_eq!(unsafe { libc::tcgetattr(0, termios.as_mut_ptr()) }, 0);
    // SAFETY: successful tcgetattr initialized termios.
    unsafe { termios.assume_init() }
}

fn assert_shell_modes(expected: &libc::termios) {
    let actual = termios();
    assert_eq!(
        (
            actual.c_iflag,
            actual.c_oflag,
            actual.c_cflag,
            actual.c_lflag,
            actual.c_cc
        ),
        (
            expected.c_iflag,
            expected.c_oflag,
            expected.c_cflag,
            expected.c_lflag,
            expected.c_cc
        ),
        "suspended/background TUI changed the foreground shell's terminal modes"
    );
}

#[test]
#[ignore = "isolated controlling-terminal fixture"]
fn job_control_driver() {
    let Some(directory) = std::env::var_os("CODEX_JOB_CONTROL_TEST_DIR") else {
        return;
    };
    let directory = Path::new(&directory);
    // SAFETY: this isolated session leader must be able to hand the terminal
    // back to the job after deliberately putting itself in the background.
    unsafe { libc::signal(libc::SIGTTOU, libc::SIG_IGN) };
    let original = termios();
    let mut command = fixture(JOB, directory);
    // SAFETY: setpgid and signal are async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            libc::signal(libc::SIGTSTP, libc::SIG_DFL);
            libc::signal(libc::SIGTTOU, libc::SIG_DFL);
            Ok(())
        });
    }
    let mut job = OwnedChild(command.spawn().expect("spawn job"));
    let pid = job.0.id() as libc::pid_t;
    std::fs::write(directory.join("job-pid"), pid.to_string()).unwrap();
    // SAFETY: this session owns the terminal and pid is its child process group.
    assert_eq!(unsafe { libc::tcsetpgrp(0, pid) }, 0);
    std::fs::write(directory.join("start"), []).unwrap();

    for cycle in 0..CYCLES {
        wait_stopped(pid);
        assert_shell_modes(&original);
        assert!(!directory.join(format!("resumed-{cycle}")).exists());
        // SAFETY: getpgrp returns this session leader's process group.
        assert_eq!(unsafe { libc::tcsetpgrp(0, libc::getpgrp()) }, 0);
        // Exercise repeated bg attempts without ever granting terminal ownership.
        for _ in 0..2 {
            // SAFETY: pid remains an owned child, retained until the end.
            assert_eq!(unsafe { libc::kill(pid, libc::SIGCONT) }, 0);
            wait_stopped(pid);
            assert_shell_modes(&original);
            assert!(!directory.join(format!("resumed-{cycle}")).exists());
        }
        // SAFETY: emulate fg: grant the terminal before continuing the job.
        assert_eq!(unsafe { libc::tcsetpgrp(0, pid) }, 0);
        assert_eq!(unsafe { libc::kill(pid, libc::SIGCONT) }, 0);
        wait_for(&directory.join(format!("resumed-{cycle}")));
        std::fs::write(directory.join(format!("next-{cycle}")), []).unwrap();
    }
    wait_for(&directory.join("finished"));
    assert_shell_modes(&original);
    assert!(job.0.wait().expect("reap job").success());
}

#[test]
#[ignore = "worker-thread job-control fixture"]
fn job_control_worker() {
    let Some(directory) = std::env::var_os("CODEX_JOB_CONTROL_TEST_DIR") else {
        return;
    };
    let directory = std::path::PathBuf::from(directory);
    wait_for(&directory.join("start"));
    std::thread::spawn(move || {
        super::super::set_modes().expect("initial terminal modes");
        for cycle in 0..CYCLES {
            super::suspend_process().expect("suspend/resume");
            std::fs::write(directory.join(format!("resumed-{cycle}")), []).unwrap();
            wait_for(&directory.join(format!("next-{cycle}")));
        }
        super::super::restore().expect("final terminal restore");
        std::fs::write(directory.join("finished"), []).unwrap();
    })
    .join()
    .expect("TUI worker");
}
