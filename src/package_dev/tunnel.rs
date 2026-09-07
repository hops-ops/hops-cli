//! Bounded, explicitly selected kubectl tunnel. Dropping it terminates kubectl.
use std::error::Error;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

pub struct PortForward {
    child: Child,
    port: u16,
    reaped: bool,
}

impl PortForward {
    pub fn start(
        context: &str,
        namespace: &str,
        service: &str,
        remote_port: u16,
        timeout: Duration,
    ) -> Result<Self, Box<dyn Error>> {
        Self::start_with("kubectl", context, namespace, service, remote_port, timeout)
    }

    fn start_with(
        program: &str,
        context: &str,
        namespace: &str,
        service: &str,
        remote_port: u16,
        timeout: Duration,
    ) -> Result<Self, Box<dyn Error>> {
        if context.trim().is_empty()
            || remote_port == 0
            || timeout.is_zero()
            || !valid_name(namespace)
            || !valid_name(service)
        {
            return Err(
                "explicit context, namespace, Service, port, and timeout are required".into(),
            );
        }
        let mut command = Command::new(program);
        command
            .args([
                "--context",
                context,
                "--namespace",
                namespace,
                "--request-timeout=15s",
                "port-forward",
                "--address=127.0.0.1",
                "--pod-running-timeout=15s",
                &format!("service/{service}"),
                &format!(":{remote_port}"),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Include auth-plugin children in cleanup without affecting the user's
        // terminal process group. No local backend or current-context mutation.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command
            .spawn()
            .map_err(|_| "could not start kubectl port-forward")?;
        let mut tunnel = Self {
            child,
            port: 0,
            reaped: false,
        };
        let stdout = tunnel
            .child
            .stdout
            .take()
            .ok_or("missing port-forward output")?;
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut output = BufReader::new(stdout);
            let mut announced = false;
            // Bounded lines: never retain arbitrary kube/auth-plugin output.
            loop {
                let mut bytes = Vec::new();
                let size = Read::by_ref(&mut output)
                    .take(1025)
                    .read_until(b'\n', &mut bytes);
                match size {
                    Ok(0) | Err(_) => break,
                    Ok(_) if bytes.len() > 1024 => break,
                    Ok(_) => {}
                }
                if let Ok(line) = std::str::from_utf8(&bytes) {
                    if let Some(port) =
                        forwarded_port(line.trim(), remote_port).filter(|_| !announced)
                    {
                        let _ = sender.send(port);
                        announced = true;
                        // Keep draining: kubectl logs each forwarded connection
                        // to stdout. Closing this pipe can terminate the tunnel.
                    }
                }
            }
        });
        tunnel.port = receiver.recv_timeout(timeout).map_err(|_| {
            "port-forward did not become ready before deadline; retry without changing the pin"
        })?;
        tunnel.check()?;
        Ok(tunnel)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn check(&mut self) -> Result<(), Box<dyn Error>> {
        if self.child.try_wait()?.is_some() {
            self.reaped = true;
            return Err("registry tunnel exited; retry without changing the pin".into());
        }
        Ok(())
    }
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn forwarded_port(line: &str, remote: u16) -> Option<u16> {
    let rest = line.strip_prefix("Forwarding from 127.0.0.1:")?;
    let (port, target) = rest.split_once(" -> ")?;
    let port = port.parse::<u16>().ok()?;
    (port != 0 && target == remote.to_string()).then_some(port)
}

impl Drop for PortForward {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        #[cfg(unix)]
        {
            // Child is not reaped until wait below, so its PID cannot be reused.
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
        }
        #[cfg(not(unix))]
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_the_expected_loopback_listener() {
        assert_eq!(
            forwarded_port("Forwarding from 127.0.0.1:32123 -> 5001", 5001),
            Some(32123)
        );
        for line in [
            "Forwarding from 0.0.0.0:32123 -> 5001",
            "Forwarding from 127.0.0.1:0 -> 5001",
            "Forwarding from 127.0.0.1:32123 -> 5000",
            "secret: synthetic-token",
        ] {
            assert_eq!(forwarded_port(line, 5001), None);
        }
    }

    #[cfg(unix)]
    #[test]
    fn startup_is_bounded_and_drop_terminates_process_group() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("hops-tunnel-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let script = root.join("kubectl");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf 'Forwarding from 127.0.0.1:32123 -> 5001\\n'\ni=0\nwhile [ $i -lt 5000 ]; do\n  printf 'Handling connection for 32123\\n'\n  i=$((i + 1))\ndone\nexec sleep 30\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut tunnel = PortForward::start_with(
            script.to_str().unwrap(),
            "remote-context",
            "development",
            "registry-upload",
            5001,
            Duration::from_secs(1),
        )
        .unwrap();
        let pid = tunnel.child.id() as i32;
        assert_eq!(tunnel.port(), 32123);
        std::thread::sleep(Duration::from_millis(100));
        tunnel.check().unwrap();
        drop(tunnel);
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        std::fs::write(
            &script,
            "#!/bin/sh\necho synthetic-secret-canary >&2\nexec sleep 30\n",
        )
        .unwrap();
        let start = std::time::Instant::now();
        let result = PortForward::start_with(
            script.to_str().unwrap(),
            "remote-context",
            "development",
            "registry-upload",
            5001,
            Duration::from_millis(100),
        );
        assert!(result.is_err());
        let error = result.err().unwrap().to_string();
        assert!(!error.contains("synthetic-secret-canary"));
        assert!(start.elapsed() < Duration::from_secs(2));
        std::fs::remove_dir_all(root).unwrap();
    }
}
