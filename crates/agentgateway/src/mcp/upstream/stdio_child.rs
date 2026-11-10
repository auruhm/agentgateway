//! Unix stdio: группа принадлежит транспорту, а не npm-обёртке.
use std::io;
use std::process::Stdio;
use std::time::Duration;

use rmcp::RoleClient;
use rmcp::model::{ClientJsonRpcMessage, ServerJsonRpcMessage};
use rmcp::transport::{Transport, async_rw::AsyncRwTransport};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

pub struct GroupChild {
	transport: AsyncRwTransport<RoleClient, ChildStdout, ChildStdin>,
	child: Child,
	pgid: Option<libc::pid_t>,
}

impl GroupChild {
	pub fn new(mut command: Command) -> io::Result<Self> {
		command.process_group(0).kill_on_drop(true);
		command
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::inherit());
		let mut child = command.spawn()?;
		let pgid = child
			.id()
			.and_then(|id| i32::try_from(id).ok())
			.filter(|id| *id > 1)
			.ok_or_else(|| io::Error::other("invalid stdio child process group"))?;
		let transport = AsyncRwTransport::new(
			child.stdout.take().expect("piped stdout"),
			child.stdin.take().expect("piped stdin"),
		);
		Ok(Self {
			transport,
			child,
			pgid: Some(pgid),
		})
	}

	fn signal(&self, signal: libc::c_int) -> io::Result<()> {
		if let Some(pgid) = self.pgid {
			// Лидер НЕ wait/try_wait до последнего сигнала: даже zombie удерживает PID
			// и исключает повторное использование PGID чужой группой.
			if unsafe { libc::kill(-pgid, signal) } != 0 {
				let error = io::Error::last_os_error();
				#[cfg(target_os = "macos")]
				if error.raw_os_error() == Some(libc::EPERM) {
					// Darwin killpg для группы только с zombie-лидером возвращает EPERM.
					// Два места отличают единственного лидера от непустой группы потомков.
					let mut members = [0 as libc::pid_t; 2];
					let count = unsafe {
						libc::proc_listpgrppids(
							pgid,
							members.as_mut_ptr().cast(),
							std::mem::size_of_val(&members) as i32,
						)
					};
					if count == 0 {
						return Ok(());
					}
					if count == 1 && members[0] == pgid {
						// getpgid на Darwin не возвращает группу zombie, но PID ещё удержан Child.
						if unsafe { libc::getpgid(pgid) } == -1
							&& io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
						{
							return Ok(());
						}
					}
				}
				if error.raw_os_error() != Some(libc::ESRCH) {
					return Err(error);
				}
			}
		}
		Ok(())
	}
}

impl Drop for GroupChild {
	fn drop(&mut self) {
		// Синхронно, в том числе при отмене close/runtime shutdown; Child reap делает Tokio.
		let _ = self.signal(libc::SIGKILL);
	}
}

impl Transport<RoleClient> for GroupChild {
	type Error = io::Error;
	fn send(
		&mut self,
		item: ClientJsonRpcMessage,
	) -> impl Future<Output = io::Result<()>> + Send + 'static {
		self.transport.send(item)
	}
	fn receive(&mut self) -> impl Future<Output = Option<ServerJsonRpcMessage>> + Send {
		self.transport.receive()
	}
	async fn close(&mut self) -> io::Result<()> {
		if self.pgid.is_none() {
			return Ok(());
		}
		let closed = tokio::time::timeout(Duration::from_secs(1), self.transport.close())
			.await
			.unwrap_or_else(|_| {
				Err(io::Error::new(
					io::ErrorKind::TimedOut,
					"stdio close blocked",
				))
			});
		tokio::time::sleep(Duration::from_secs(3)).await;
		self.signal(libc::SIGTERM)?;
		tokio::time::sleep(Duration::from_millis(200)).await;
		self.signal(libc::SIGKILL)?;
		// После последнего group kill больше не используем числовой PGID.
		self.pgid = None;
		self.child.wait().await?;
		closed
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn stdio_group_close_and_cancel_reap_descendants_only() {
		let mut unrelated = Command::new("/bin/sleep")
			.arg("60")
			.kill_on_drop(true)
			.spawn()
			.unwrap();
		for cancel in [false, true] {
			let path = std::env::temp_dir().join(format!("agw-stdio-{}-{cancel}", std::process::id()));
			let script = r#"import os,signal,subprocess,sys,time
signal.signal(signal.SIGTERM,signal.SIG_IGN)
p = subprocess.Popen([sys.executable, "-c", "import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); time.sleep(60)"])
open(sys.argv[1],"w").write(str(p.pid))
while True: time.sleep(1)
"#;
			let mut command = Command::new("python3");
			command.arg("-c").arg(script).arg(&path);
			let mut transport = GroupChild::new(command).unwrap();
			let leader = transport.child.id().unwrap();
			let grandchild = tokio::time::timeout(Duration::from_secs(5), async {
				loop {
					if let Ok(text) = std::fs::read_to_string(&path) {
						if let Ok(pid) = text.parse::<i32>() {
							break pid;
						}
					}
					tokio::time::sleep(Duration::from_millis(20)).await;
				}
			})
			.await
			.unwrap();
			if cancel {
				let mut closing = Box::pin(Transport::close(&mut transport));
				tokio::select! {
						_ = &mut closing => panic!("close unexpectedly completed"),
						_ = tokio::time::sleep(Duration::from_millis(50)) => {},
				}
				drop(closing);
				drop(transport);
			} else {
				Transport::close(&mut transport).await.unwrap();
				drop(transport);
			}
			tokio::time::timeout(Duration::from_secs(5), async {
				loop {
					let leader_alive = unsafe { libc::kill(leader as i32, 0) } == 0;
					let grandchild_alive = unsafe { libc::kill(grandchild, 0) } == 0;
					if !leader_alive && !grandchild_alive {
						break;
					}
					tokio::time::sleep(Duration::from_millis(20)).await;
				}
			})
			.await
			.expect("stdio child/grandchild survived cleanup");
			assert!(
				unrelated.try_wait().unwrap().is_none(),
				"unrelated process killed"
			);
			std::fs::remove_file(path).unwrap();
		}
		unrelated.kill().await.unwrap();
		let mut exited = GroupChild::new(Command::new("/usr/bin/true")).unwrap();
		tokio::time::sleep(Duration::from_millis(50)).await;
		Transport::close(&mut exited)
			.await
			.expect("already exited leader cleanup");
	}
}
