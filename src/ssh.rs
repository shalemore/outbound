use std::{
    ffi::OsString,
    io::{BufRead, BufReader, Read, Seek, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio},
    sync::{Mutex, OnceLock},
    time::Instant,
};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

use crate::{
    config,
    sync::{
        CHUNK_SIZE, Cancelled, Permanent, Remote, RemoteEntry, UploadControl, open_local, throttle,
        to_hex,
    },
};

const SPAWN_HINT: &str = "cannot start ssh, make sure an OpenSSH client is installed";

/// Runs the system ssh, so authentication (ssh-agent, keys, ProxyJump) and known_hosts work the
/// way they do in a terminal. The remote must be a Linux server with `sh` and `sha256sum`; the
/// first command of a connection checks that.
///
/// The short commands share one connection, which stays open as long as this value lives. An
/// upload streams over a connection of its own.
pub struct SshRemote {
    /// The program that is run, and the arguments that precede the ssh options. Normally this is
    /// plain `ssh`; the tests substitute a script that runs the remote command locally.
    program: OsString,
    leading_arguments: Vec<OsString>,
    destination: String,
    port: Option<u16>,
    identity_file: Option<PathBuf>,
    /// Set once the remote has been found to be supported.
    supported: OnceLock<()>,
    session: Mutex<Option<Session>>,
}

/// A remote `sh` that reads one script after another from its stdin.
struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// Printed after every script with its exit status, so that the end of its output can be found.
const END: &str = "--outbound-end--";

impl Session {
    /// The script runs in a subshell, so that an `exit` in it ends the script and not the session,
    /// and its errors are part of its output.
    fn run(&mut self, script: &str) -> Result<(i32, String)> {
        write!(
            self.stdin,
            "(\n{script}\n) 2>&1\nprintf '\\n%s %s\\n' {END} $?\n"
        )?;
        self.stdin.flush()?;
        let mut output = Vec::new();
        let mut line = Vec::new();
        loop {
            line.clear();
            if self.stdout.read_until(b'\n', &mut line)? == 0 {
                bail!("the connection was closed");
            }
            if let Some(status) = line.strip_prefix(END.as_bytes()) {
                let status = String::from_utf8_lossy(status).trim().parse().unwrap_or(-1);
                // The newline that was printed before the marker is not part of the output.
                if output.ends_with(b"\n") {
                    output.pop();
                }
                return Ok((status, String::from_utf8_lossy(&output).into_owned()));
            }
            output.extend_from_slice(&line);
        }
    }

    /// Ends the connection, and returns what ssh said about it, which explains a failure.
    fn close(mut self) -> String {
        drop(self.stdin);
        let _ = self.child.kill();
        let mut errors = String::new();
        if let Some(mut stderr) = self.child.stderr.take() {
            let _ = stderr.read_to_string(&mut errors);
        }
        let _ = self.child.wait();
        errors.trim().to_string()
    }
}

impl Drop for SshRemote {
    fn drop(&mut self) {
        if let Some(session) = self.session.get_mut().ok().and_then(Option::take) {
            session.close();
        }
    }
}

impl SshRemote {
    pub fn new(ssh: &config::Ssh) -> Self {
        Self {
            program: "ssh".into(),
            leading_arguments: Vec::new(),
            destination: ssh.destination.trim().to_string(),
            port: ssh.port,
            identity_file: ssh.identity_file.clone(),
            supported: OnceLock::new(),
            session: Mutex::new(None),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.leading_arguments);
        // BatchMode keeps a background run from hanging on a password prompt, and the keepalive
        // options make a dropped connection show up quickly.
        command.args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=15",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
        ]);
        if let Some(port) = self.port {
            command.arg("-p").arg(port.to_string());
        }
        if let Some(identity_file) = &self.identity_file {
            command.arg("-i").arg(identity_file);
        }
        command.arg(&self.destination);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // The tray app has no console, so without this flag every ssh call flashes a black window.
            command.creation_flags(0x0800_0000);
        }
        command
    }

    /// The script goes to the remote `sh -s` on stdin, so paths never pass through command line
    /// escaping.
    fn run_script(&self, script: &str) -> Result<String> {
        self.ensure_supported()?;
        self.run_unchecked(script)
    }

    /// Only Linux is supported, because the scripts rely on GNU or BusyBox tools. macOS, the BSDs
    /// and Windows servers fail here with a clear message instead of somewhere in the middle of an
    /// upload.
    fn ensure_supported(&self) -> Result<()> {
        if self.supported.get().is_some() {
            return Ok(());
        }
        let output = self.run_unchecked(PLATFORM_PROBE)?;
        check_platform(&output)?;
        let _ = self.supported.set(());
        Ok(())
    }

    fn run_unchecked(&self, script: &str) -> Result<String> {
        let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
        if session.is_none() {
            *session = Some(self.open_session()?);
        }
        let result = session.as_mut().map(|open| open.run(script));
        match result {
            Some(Ok((0, output))) => Ok(output),
            // The connection works, so asking again gets the same answer.
            Some(Ok((status, output))) => Err(Permanent(format!(
                "a command on the remote failed ({status}): {}",
                output.trim()
            ))
            .into()),
            // The connection is gone. Whatever ssh said is the better explanation, and the next
            // command connects again.
            Some(Err(e)) => {
                let said = session.take().map(Session::close).unwrap_or_default();
                if said.is_empty() {
                    Err(e.context("ssh failed"))
                } else {
                    bail!("ssh failed: {said}")
                }
            }
            None => unreachable!("the session was opened above"),
        }
    }

    fn open_session(&self) -> Result<Session> {
        let mut child = self
            .command()
            .args(["sh", "-s"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Permanent(format!("{SPAWN_HINT}: {e}")))?;
        let stdin = child.stdin.take().context("cannot open the stdin of ssh")?;
        let stdout = child
            .stdout
            .take()
            .context("cannot open the stdout of ssh")?;
        Ok(Session {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    /// Removes the remote temporary file when it can no longer be resumed. If this fails too it
    /// does not matter, because the next upload overwrites the file.
    fn remove_temp(&self, temp: &str) {
        let _ = self.run_script(&format!("rm -f -- {}\n", sh_quote(temp)));
    }

    /// The home directory of the login user on the remote.
    pub fn home_dir(&self) -> Result<String> {
        let output = self.run_script("echo \"$HOME\"\n")?;
        let home = last_line(&output);
        if !home.starts_with('/') {
            bail!("unexpected answer from the remote: {output}");
        }
        Ok(home.to_string())
    }

    /// Looks at a folder path that is being typed: whether it is a folder, and which folders are
    /// in the folder the path points into, or, for a path without a trailing slash, in its parent.
    pub fn browse(&self, path: &str) -> Result<Browse> {
        let listed = listed_folder(path);
        let script = format!(
            "if [ -d {} ]; then echo yes; else echo no; fi
             ls -1Ap {} 2>/dev/null | sed -n 's|/$||p'
",
            sh_quote(path),
            sh_quote(&listed)
        );
        let output = self.run_script(&script)?;
        let mut lines = output.lines();
        let exists = lines.next().map(str::trim) == Some("yes");
        let mut folders: Vec<String> = lines
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(String::from)
            .collect();
        folders.sort_by_key(|name| name.to_lowercase());
        Ok(Browse {
            exists,
            listed,
            folders,
        })
    }

    fn partial_size(&self, temp: &str) -> Result<u64> {
        let temp = sh_quote(temp);
        let output = self.run_script(&format!(
            "if [ -f {temp} ]; then wc -c < {temp}; else echo 0; fi\n"
        ))?;
        last_line(&output)
            .parse()
            .with_context(|| format!("cannot parse the size of the partial file: {output}"))
    }

    /// Decides how much of an interrupted upload can be kept, like `rsync --append-verify`. The
    /// partial remote file counts only if it is a prefix of the local file, which is checked by
    /// hashing both sides, so a stale or corrupted leftover never ends up in the final file.
    /// Returns the number of bytes to skip. `hasher` has consumed exactly those bytes and `file`
    /// is positioned right after them.
    fn reusable_prefix(
        &self,
        file: &mut (impl Read + Seek),
        hasher: &mut Sha256,
        temp: &str,
        expected_size: u64,
        progress: &mut dyn FnMut(u64),
    ) -> Result<u64> {
        let existing = self.partial_size(temp)?;
        if existing == 0 || existing > expected_size {
            return Ok(0);
        }
        hash_prefix(file, existing, hasher, progress)?;
        if to_hex(&hasher.clone().finalize()) == self.sha256(temp)? {
            log::info!("resuming an interrupted upload at {existing} of {expected_size} bytes");
            return Ok(existing);
        }
        log::warn!("discarding a partial upload that does not match the local file");
        *hasher = Sha256::new();
        file.rewind()?;
        Ok(0)
    }
}

impl Remote for SshRemote {
    fn dir_exists(&self, path: &str) -> Result<bool> {
        let script = format!(
            "if [ -d {} ]; then echo yes; else echo no; fi\n",
            sh_quote(path)
        );
        Ok(last_line(&self.run_script(&script)?) == "yes")
    }

    fn stat(&self, path: &str) -> Result<RemoteEntry> {
        let path = sh_quote(path);
        let script = format!(
            "if [ -f {path} ]; then echo \"file $(wc -c < {path})\"; \
             elif [ -e {path} ] || [ -L {path} ]; then echo other; else echo none; fi\n"
        );
        let output = self.run_script(&script)?;
        match last_line(&output).split_once(' ') {
            Some(("file", size)) => Ok(RemoteEntry::File {
                size: size
                    .trim()
                    .parse()
                    .context("cannot parse the remote file size")?,
            }),
            None if last_line(&output) == "other" => Ok(RemoteEntry::Other),
            None if last_line(&output) == "none" => Ok(RemoteEntry::Missing),
            _ => bail!("unexpected answer from the remote: {output}"),
        }
    }

    fn sha256(&self, path: &str) -> Result<String> {
        let output = self.run_script(&format!("sha256sum -- {}\n", sh_quote(path)))?;
        parse_sha256(&output)
    }

    fn upload(
        &self,
        local: &Path,
        remote: &str,
        expected_size: u64,
        control: &UploadControl,
        progress: &mut dyn FnMut(u64),
    ) -> Result<()> {
        self.ensure_supported()?;
        let (directory, _) = remote
            .rsplit_once('/')
            .context("the remote path must be absolute")?;
        let directory = if directory.is_empty() { "/" } else { directory };
        let temp = temp_path(directory, remote);

        let mut file = open_local(local)?;
        let mut hasher = Sha256::new();
        let offset =
            self.reusable_prefix(&mut file, &mut hasher, &temp, expected_size, progress)?;

        let redirect = if offset > 0 { ">>" } else { ">" };
        let mut child = self
            .command()
            .args(["sh", "-c"])
            .arg(sh_quote(&format!(
                "mkdir -p {} && cat {redirect} {}",
                sh_quote(directory),
                sh_quote(&temp)
            )))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Permanent(format!("{SPAWN_HINT}: {e}")))?;
        let mut stdin = child.stdin.take().context("cannot open the stdin of ssh")?;
        let pumped = pump(
            &mut file,
            &mut stdin,
            &mut hasher,
            offset,
            control,
            progress,
        );
        // stdin must be closed first, otherwise the remote cat never sees EOF and never exits.
        drop(stdin);
        let output = child.wait_with_output()?;

        // After a transfer failure the temporary file stays on the remote, so the next attempt
        // resumes from what has already arrived.
        let sent = match pumped {
            Ok(sent) => sent,
            // Stopped on purpose, for example by pausing. The partial file stays for the resume.
            Err(e) if e.is::<Cancelled>() => return Err(e),
            Err(e) => {
                let message = format!("{e:#} (ssh: {})", stderr_text(&output.stderr));
                return Err(if remote_command_failed(&output.status) {
                    Permanent(message).into()
                } else {
                    anyhow!(message)
                });
            }
        };
        if !output.status.success() {
            let message = format!(
                "ssh failed ({}): {}",
                output.status,
                stderr_text(&output.stderr)
            );
            return Err(if remote_command_failed(&output.status) {
                Permanent(message).into()
            } else {
                anyhow!(message)
            });
        }
        if sent != expected_size {
            self.remove_temp(&temp);
            return Err(Permanent(format!(
                "the file changed during the upload (scanned as {expected_size} bytes, read {sent} bytes)"
            ))
            .into());
        }

        let local_hash = to_hex(&hasher.finalize());
        let outcome = self.run_script(&commit_script(&temp, remote, &local_hash))?;
        match last_line(&outcome) {
            "OK" => Ok(()),
            "MISMATCH" => bail!(
                "remote verification failed, the temporary file was discarded and the next cycle retries"
            ),
            "EXISTS" => Err(Permanent(
                "a file with this name appeared on the remote during the upload, the temporary file was discarded"
                    .to_string(),
            )
            .into()),
            _ => bail!("unexpected answer from the remote: {outcome}"),
        }
    }
}

/// ssh exits with 255 when the connection fails and otherwise with the status of the remote
/// command. A command that failed, for example for lack of space or permission, fails again.
fn remote_command_failed(status: &ExitStatus) -> bool {
    matches!(status.code(), Some(code) if code != 0 && code != 255)
}

/// Feeds the rest of the local file into the stdin of ssh and hashes it on the way, so the hash
/// of the whole file can be compared with the one computed on the remote after the file landed on
/// disk. `sent` is the number of bytes that are already on the remote, and the total is returned.
/// The control is looked at before every chunk, and a speed limit makes the loop wait after it.
fn pump(
    file: &mut impl Read,
    stdin: &mut impl Write,
    hasher: &mut Sha256,
    mut sent: u64,
    control: &UploadControl,
    progress: &mut dyn FnMut(u64),
) -> Result<u64> {
    let mut buffer = vec![0; CHUNK_SIZE];
    let (started, already_sent) = (Instant::now(), sent);
    loop {
        if (control.cancel)() {
            return Err(Cancelled.into());
        }
        let read = file
            .read(&mut buffer)
            .context("cannot read the local file")?;
        if read == 0 {
            return Ok(sent);
        }
        hasher.update(&buffer[..read]);
        stdin
            .write_all(&buffer[..read])
            .context("cannot write to ssh")?;
        sent += read as u64;
        progress(sent);
        if let Some(limit) = control.bytes_per_sec {
            throttle(started, sent - already_sent, limit, control.cancel)?;
        }
    }
}

/// Hashes the first `len` bytes of the file.
fn hash_prefix(
    file: &mut impl Read,
    len: u64,
    hasher: &mut Sha256,
    progress: &mut dyn FnMut(u64),
) -> Result<()> {
    let mut buffer = vec![0; CHUNK_SIZE];
    let mut done = 0;
    while done < len {
        let wanted = usize::try_from((len - done).min(CHUNK_SIZE as u64))?;
        let read = file
            .read(&mut buffer[..wanted])
            .context("cannot read the local file")?;
        if read == 0 {
            bail!("the local file is shorter than the partial file on the remote");
        }
        hasher.update(&buffer[..read]);
        done += read as u64;
        progress(done);
    }
    Ok(())
}

/// The temporary name is ASCII only and independent of the target name, so it can never exceed
/// the file name length limit.
fn temp_path(directory: &str, remote: &str) -> String {
    let digest = to_hex(&Sha256::digest(remote.as_bytes()));
    format!("{directory}/.outbound-{}.part", &digest[..16])
}

/// Verifies the hash, makes sure the destination does not exist and only then renames. If any
/// step fails the temporary file is removed.
fn commit_script(temp: &str, destination: &str, expected_hash: &str) -> String {
    format!(
        "set -e\n\
         temp={}\n\
         destination={}\n\
         actual=$(sha256sum -- \"$temp\" | cut -d' ' -f1)\n\
         if [ \"$actual\" != '{expected_hash}' ]; then rm -f -- \"$temp\"; echo MISMATCH; exit 0; fi\n\
         if [ -e \"$destination\" ]; then rm -f -- \"$temp\"; echo EXISTS; exit 0; fi\n\
         mv -- \"$temp\" \"$destination\"\n\
         echo OK\n",
        sh_quote(temp),
        sh_quote(destination),
    )
}

fn parse_sha256(output: &str) -> Result<String> {
    let hash = last_line(output)
        .split_whitespace()
        .next()
        .unwrap_or_default();
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("cannot parse the sha256sum output of the remote: {output}");
    }
    Ok(hash.to_ascii_lowercase())
}

/// A login script may print extra text before the answer, so only the last line counts.
/// Asks the remote what it is and whether it has the tool that the scripts need.
const PLATFORM_PROBE: &str = "uname -s
if command -v sha256sum >/dev/null 2>&1; then echo has-sha256sum; fi
";

/// Reads the answer to `PLATFORM_PROBE`. A login script may print extra lines, so the answers are
/// looked for, not counted.
fn check_platform(output: &str) -> Result<()> {
    let says = |answer: &str| output.lines().any(|line| line.trim() == answer);
    if !says("Linux") {
        return Err(Permanent(
            "the server is not running Linux, which is the only supported system".into(),
        )
        .into());
    }
    if !says("has-sha256sum") {
        return Err(Permanent("the server has no sha256sum".into()).into());
    }
    Ok(())
}

/// What `SshRemote::browse` found.
#[derive(Debug, PartialEq)]
pub struct Browse {
    /// The path is a folder on the remote.
    pub exists: bool,
    /// The folder whose content is listed, with no trailing slash except for the root.
    pub listed: String,
    /// The names of the folders in it.
    pub folders: Vec<String>,
}

impl Browse {
    /// The folders that fit the part typed after the last slash. Hidden folders are only offered
    /// to someone who starts the name with a dot.
    pub fn matching(&self, typed: &str) -> Vec<&str> {
        self.folders
            .iter()
            .map(String::as_str)
            .filter(|name| {
                name.starts_with(typed) && (typed.starts_with('.') || !name.starts_with('.'))
            })
            .collect()
    }

    /// The path of one of the listed folders, ready to be typed on.
    pub fn path_of(&self, name: &str) -> String {
        match self.listed.as_str() {
            "/" => format!("/{name}/"),
            listed => format!("{listed}/{name}/"),
        }
    }

    /// The folder above the listed one, None at the root.
    pub fn parent_path(&self) -> Option<String> {
        if self.listed == "/" {
            return None;
        }
        let (parent, _) = self.listed.rsplit_once('/')?;
        Some(format!("{parent}/"))
    }

    /// What Tab makes of a typed path, as in a shell: the only folder that fits, or as much of the
    /// name as all the folders that fit share. None when it adds nothing, or when the path no
    /// longer points into the listed folder.
    pub fn complete(&self, typed: &str) -> Option<String> {
        if listed_folder(typed) != self.listed {
            return None;
        }
        let name = typed.rsplit('/').next().unwrap_or_default();
        match self.matching(name).as_slice() {
            [] => None,
            [only] => Some(self.path_of(only)),
            [first, rest @ ..] => {
                let shared = rest.iter().fold(first.len(), |shared, other| {
                    first
                        .char_indices()
                        .zip(other.chars())
                        .take_while(|((_, a), b)| a == b)
                        .last()
                        .map_or(0, |((at, c), _)| at + c.len_utf8())
                        .min(shared)
                });
                (shared > name.len())
                    .then(|| format!("{}{}", &typed[..typed.len() - name.len()], &first[..shared]))
            }
        }
    }
}

/// The folder that a typed path points into: the path itself when it ends with a slash, and
/// otherwise the folder that holds the last part.
fn listed_folder(path: &str) -> String {
    let folder = match path.rsplit_once('/') {
        Some((folder, _)) => folder,
        None => "",
    };
    if folder.is_empty() { "/" } else { folder }.to_string()
}

fn last_line(output: &str) -> &str {
    output.lines().last().unwrap_or_default().trim()
}

fn stderr_text(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr).trim().to_string()
}

/// Wraps the text in single quotes, writing every inner single quote as '\''.
pub fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use super::*;

    fn never() -> bool {
        false
    }

    /// No cancelling and no speed limit.
    fn free() -> UploadControl<'static> {
        UploadControl {
            cancel: &never,
            bytes_per_sec: None,
        }
    }

    #[test]
    fn lists_the_folder_a_typed_path_points_into() {
        assert_eq!(listed_folder("/data/vid"), "/data");
        assert_eq!(listed_folder("/data/"), "/data");
        assert_eq!(listed_folder("/data"), "/");
        assert_eq!(listed_folder("/"), "/");
        assert_eq!(listed_folder(""), "/");
    }

    #[test]
    fn offers_the_folders_that_fit_what_is_typed() {
        let browse = Browse {
            exists: false,
            listed: "/data".into(),
            folders: vec![
                ".cache".into(),
                "movies".into(),
                "music".into(),
                "tv".into(),
            ],
        };
        assert_eq!(browse.matching(""), ["movies", "music", "tv"]);
        assert_eq!(browse.matching("mu"), ["music"]);
        assert_eq!(browse.matching(".c"), [".cache"]);
        assert_eq!(browse.path_of("tv"), "/data/tv/");
        assert_eq!(browse.parent_path().as_deref(), Some("/"));

        let root = Browse {
            listed: "/".into(),
            ..browse
        };
        assert_eq!(root.path_of("data"), "/data/");
        assert_eq!(root.parent_path(), None);
        let deep = Browse {
            listed: "/data/tv".into(),
            ..root
        };
        assert_eq!(deep.parent_path().as_deref(), Some("/data/"));
    }

    #[test]
    fn tab_completes_like_a_shell() {
        let browse = Browse {
            exists: false,
            listed: "/data".into(),
            folders: vec![
                "movies".into(),
                "music".into(),
                "tv".into(),
                "téléfilms".into(),
            ],
        };
        assert_eq!(
            browse.complete("/data/t").as_deref(),
            None,
            "tv and téléfilms share only t"
        );
        assert_eq!(browse.complete("/data/tv").as_deref(), Some("/data/tv/"));
        assert_eq!(
            browse.complete("/data/mo").as_deref(),
            Some("/data/movies/")
        );
        assert_eq!(
            browse.complete("/data/m").as_deref(),
            None,
            "movies and music share only m"
        );
        assert_eq!(
            browse.complete("/data/té").as_deref(),
            Some("/data/téléfilms/")
        );
        assert_eq!(browse.complete("/data/x").as_deref(), None);
        assert_eq!(browse.complete("/other/m").as_deref(), None);

        let shared = Browse {
            folders: vec!["season 1".into(), "season 2".into()],
            ..browse
        };
        assert_eq!(shared.complete("/data/s").as_deref(), Some("/data/season "));
    }

    /// Talks to a real server through one session. Run it with the ssh destination in
    /// OUTBOUND_TEST_HOST: `cargo test --release real_server -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_server() {
        let destination = std::env::var("OUTBOUND_TEST_HOST").expect("OUTBOUND_TEST_HOST");
        let remote = SshRemote::new(&config::Ssh {
            destination,
            port: None,
            identity_file: None,
        });
        for round in 1..=3 {
            let started = Instant::now();
            let home = remote.home_dir().unwrap();
            let browse = remote.browse(&format!("{home}/")).unwrap();
            assert!(browse.exists);
            eprintln!(
                "round {round}: {:?}, {} folders",
                started.elapsed(),
                browse.folders.len()
            );
        }

        // An upload into a folder that is created for it in /tmp, and removed again.
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("file.bin");
        let data = patterned(CHUNK_SIZE * 3 + 77);
        std::fs::write(&local, &data).unwrap();
        let folder = format!("/tmp/outbound-test-{}", std::process::id());
        let destination = format!("{folder}/sub/file.bin");
        let size = data.len() as u64;
        let started = Instant::now();
        remote
            .upload(&local, &destination, size, &free(), &mut |_| {})
            .unwrap();
        eprintln!("upload of {size} bytes: {:?}", started.elapsed());
        assert!(matches!(
            remote.stat(&destination).unwrap(),
            RemoteEntry::File { size: found } if found == size
        ));
        assert_eq!(
            remote.sha256(&destination).unwrap(),
            to_hex(&Sha256::digest(&data))
        );
        remote
            .run_script(&format!("rm -rf -- {}\n", sh_quote(&folder)))
            .unwrap();
        assert!(!remote.dir_exists(&folder).unwrap());
    }

    #[test]
    fn only_linux_with_sha256sum_is_supported() {
        assert!(
            check_platform(
                "Welcome!
Linux
has-sha256sum
"
            )
            .is_ok()
        );
        let error = check_platform(
            "Darwin
",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("not running Linux"), "{error}");
        let error = check_platform(
            "Linux
",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("no sha256sum"), "{error}");
    }

    #[test]
    fn quotes_shell_strings() {
        assert_eq!(sh_quote("/data/a b"), "'/data/a b'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_quote("$(rm -rf /)"), "'$(rm -rf /)'");
    }

    #[test]
    fn temp_name_is_ascii_and_stable() {
        let a = temp_path("/data", "/data/vidéo.mkv");
        assert!(a.is_ascii());
        assert_eq!(a, temp_path("/data", "/data/vidéo.mkv"));
        assert_ne!(a, temp_path("/data", "/data/other.mkv"));
    }

    #[test]
    fn parses_sha256_output() {
        let hash = "a".repeat(64);
        assert_eq!(parse_sha256(&format!("{hash}  /x\n")).unwrap(), hash);
        assert!(parse_sha256("sha256sum: /x: No such file").is_err());
    }

    #[test]
    fn pump_counts_and_hashes() {
        let mut sink = Vec::new();
        let mut reported = 0;
        let mut hasher = Sha256::new();
        let sent = pump(
            &mut &b"hello"[..],
            &mut sink,
            &mut hasher,
            0,
            &free(),
            &mut |done| {
                reported = done;
            },
        )
        .unwrap();
        assert_eq!((sent, reported), (5, 5));
        assert_eq!(sink, b"hello");
        assert_eq!(
            to_hex(&hasher.finalize()),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn prefix_and_rest_hash_like_the_whole_file() {
        let data = patterned(CHUNK_SIZE * 2 + 123);
        let split = CHUNK_SIZE + 7;

        let mut hasher = Sha256::new();
        let mut file = &data[..];
        hash_prefix(&mut file, split as u64, &mut hasher, &mut |_| {}).unwrap();
        let sent = pump(
            &mut file,
            &mut Vec::new(),
            &mut hasher,
            split as u64,
            &free(),
            &mut |_| {},
        )
        .unwrap();

        assert_eq!(sent, data.len() as u64);
        assert_eq!(to_hex(&hasher.finalize()), to_hex(&Sha256::digest(&data)));
    }

    #[test]
    fn prefix_longer_than_the_file_is_an_error() {
        let mut hasher = Sha256::new();
        assert!(hash_prefix(&mut &b"abc"[..], 4, &mut hasher, &mut |_| {}).is_err());
    }

    #[test]
    fn commit_script_embeds_quoted_paths() {
        let script = commit_script("/d/.t.part", "/d/it's.mkv", "abc");
        assert!(script.contains(r"destination='/d/it'\''s.mkv'"));
        assert!(script.contains("'abc'"));
    }

    fn patterned(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// A stand-in for ssh: it drops the ssh options and the destination and runs the remote
    /// command locally, the way the remote shell would parse it.
    const FAKE_SSH: &str = "\
while [ \"$1\" = -o ] || [ \"$1\" = -p ] || [ \"$1\" = -i ]; do shift 2; done\n\
shift\n\
exec sh -c \"$*\"\n";

    /// The "remote" is a temporary directory on this machine, reached through the real shell
    /// scripts of `SshRemote`.
    struct Fixture {
        _dir: tempfile::TempDir,
        remote: SshRemote,
        /// The remote folder, with forward slashes so that it also works in a Windows sh.
        root: String,
        local: PathBuf,
        data: Vec<u8>,
    }

    impl Fixture {
        /// Returns None when there is no `sh` to run the scripts with.
        fn new() -> Option<Self> {
            let sh_available = Command::new("sh")
                .args(["-c", "exit 0"])
                .status()
                .is_ok_and(|status| status.success());
            if !sh_available {
                eprintln!("skipping: no sh on this machine");
                return None;
            }
            let dir = tempfile::tempdir().unwrap();
            let script = dir.path().join("fake-ssh.sh");
            std::fs::write(&script, FAKE_SSH).unwrap();
            let data = patterned(CHUNK_SIZE * 2 + 4321);
            let local = dir.path().join("source.bin");
            std::fs::write(&local, &data).unwrap();
            let root = dir
                .path()
                .join("remote")
                .to_string_lossy()
                .replace('\\', "/");

            let mut remote = SshRemote::new(&config::Ssh {
                destination: "fake".into(),
                port: None,
                identity_file: None,
            });
            remote.program = "sh".into();
            // The stand-in runs the scripts here, and this machine need not be Linux.
            let _ = remote.supported.set(());
            remote.leading_arguments = vec![script.to_string_lossy().replace('\\', "/").into()];
            Some(Self {
                _dir: dir,
                remote,
                root,
                local,
                data,
            })
        }

        fn destination(&self) -> String {
            format!("{}/sub/file.bin", self.root)
        }

        fn temp(&self) -> String {
            temp_path(&format!("{}/sub", self.root), &self.destination())
        }

        fn write_partial(&self, bytes: &[u8]) {
            std::fs::create_dir_all(format!("{}/sub", self.root)).unwrap();
            std::fs::write(self.temp(), bytes).unwrap();
        }

        fn reusable_prefix(&self) -> u64 {
            let mut file = File::open(&self.local).unwrap();
            let mut hasher = Sha256::new();
            let offset = self
                .remote
                .reusable_prefix(
                    &mut file,
                    &mut hasher,
                    &self.temp(),
                    self.data.len() as u64,
                    &mut |_| {},
                )
                .unwrap();
            // The hasher must have consumed exactly the bytes that are skipped.
            let mut expected = Sha256::new();
            expected.update(&self.data[..usize::try_from(offset).unwrap()]);
            assert_eq!(to_hex(&hasher.finalize()), to_hex(&expected.finalize()));
            offset
        }

        fn upload(&self) -> Result<()> {
            self.remote.upload(
                &self.local,
                &self.destination(),
                self.data.len() as u64,
                &free(),
                &mut |_| {},
            )
        }

        fn uploaded(&self) -> Vec<u8> {
            std::fs::read(self.destination()).unwrap()
        }

        fn temp_exists(&self) -> bool {
            Path::new(&self.temp()).exists()
        }
    }

    #[test]
    fn uploads_through_the_shell_scripts() {
        let Some(fx) = Fixture::new() else { return };
        let mut last = 0;
        fx.remote
            .upload(
                &fx.local,
                &fx.destination(),
                fx.data.len() as u64,
                &free(),
                &mut |done| {
                    last = done;
                },
            )
            .unwrap();

        assert_eq!(fx.uploaded(), fx.data);
        assert_eq!(last, fx.data.len() as u64);
        assert!(!fx.temp_exists());
    }

    #[test]
    fn resumes_a_partial_file_that_matches() {
        let Some(fx) = Fixture::new() else { return };
        let kept = CHUNK_SIZE + 99;
        fx.write_partial(&fx.data[..kept]);

        assert_eq!(fx.reusable_prefix(), kept as u64);
        fx.upload().unwrap();
        assert_eq!(fx.uploaded(), fx.data);
        assert!(!fx.temp_exists());
    }

    #[test]
    fn resumes_when_the_partial_file_is_already_complete() {
        let Some(fx) = Fixture::new() else { return };
        fx.write_partial(&fx.data);

        assert_eq!(fx.reusable_prefix(), fx.data.len() as u64);
        fx.upload().unwrap();
        assert_eq!(fx.uploaded(), fx.data);
    }

    #[test]
    fn restarts_when_the_partial_file_is_not_a_prefix() {
        let Some(fx) = Fixture::new() else { return };
        let mut wrong = fx.data[..5000].to_vec();
        wrong[4000] ^= 0xff;
        fx.write_partial(&wrong);

        assert_eq!(fx.reusable_prefix(), 0);
        fx.upload().unwrap();
        assert_eq!(fx.uploaded(), fx.data);
    }

    #[test]
    fn restarts_when_the_partial_file_is_longer_than_the_source() {
        let Some(fx) = Fixture::new() else { return };
        let mut longer = fx.data.clone();
        longer.extend_from_slice(b"extra");
        fx.write_partial(&longer);

        assert_eq!(fx.reusable_prefix(), 0);
        fx.upload().unwrap();
        assert_eq!(fx.uploaded(), fx.data);
    }

    #[test]
    fn never_overwrites_an_existing_destination() {
        let Some(fx) = Fixture::new() else { return };
        std::fs::create_dir_all(format!("{}/sub", fx.root)).unwrap();
        std::fs::write(fx.destination(), b"someone else's file").unwrap();

        let error = fx.upload().unwrap_err();
        assert!(format!("{error:#}").contains("appeared"), "{error:#}");
        assert_eq!(fx.uploaded(), b"someone else's file");
        assert!(!fx.temp_exists());
    }

    #[test]
    fn stat_and_hash_answer_for_files_directories_and_missing_paths() {
        let Some(fx) = Fixture::new() else { return };
        fx.write_partial(b"hello");
        let file = fx.temp();

        assert!(matches!(
            fx.remote.stat(&file).unwrap(),
            RemoteEntry::File { size: 5 }
        ));
        assert!(matches!(
            fx.remote.stat(&format!("{}/sub", fx.root)).unwrap(),
            RemoteEntry::Other
        ));
        assert!(matches!(
            fx.remote.stat(&format!("{}/nope", fx.root)).unwrap(),
            RemoteEntry::Missing
        ));
        assert!(fx.remote.dir_exists(&format!("{}/sub", fx.root)).unwrap());
        assert!(!fx.remote.dir_exists(&file).unwrap());

        let inside = fx.remote.browse(&format!("{}/", fx.root)).unwrap();
        assert!(inside.exists);
        assert_eq!(inside.folders, ["sub"]);
        let typing = fx.remote.browse(&format!("{}/su", fx.root)).unwrap();
        assert!(!typing.exists);
        assert_eq!(typing.listed, fx.root);
        assert_eq!(typing.folders, ["sub"]);
        assert!(
            !fx.remote
                .browse(&format!("{}/nope/x", fx.root))
                .unwrap()
                .exists
        );
        assert_eq!(
            fx.remote.sha256(&file).unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn pump_stops_when_cancelled() {
        let mut sink = Vec::new();
        let mut hasher = Sha256::new();
        let error = pump(
            &mut &b"hello"[..],
            &mut sink,
            &mut hasher,
            0,
            &UploadControl {
                cancel: &|| true,
                bytes_per_sec: None,
            },
            &mut |_| {},
        )
        .unwrap_err();
        assert!(error.is::<Cancelled>());
        assert!(sink.is_empty());
    }

    #[test]
    fn a_cancelled_upload_resumes_where_it_stopped() {
        let Some(fx) = Fixture::new() else { return };
        // Stop once the first chunk has been sent.
        let sent = std::cell::Cell::new(0);
        let error = fx
            .remote
            .upload(
                &fx.local,
                &fx.destination(),
                fx.data.len() as u64,
                &UploadControl {
                    cancel: &|| sent.get() >= CHUNK_SIZE as u64,
                    bytes_per_sec: None,
                },
                &mut |done| sent.set(done),
            )
            .unwrap_err();
        assert!(error.is::<Cancelled>(), "{error:#}");
        assert!(fx.temp_exists(), "the partial file is kept");
        assert!(!Path::new(&fx.destination()).exists());

        let kept = std::fs::metadata(fx.temp()).unwrap().len();
        assert!(kept > 0 && kept < fx.data.len() as u64);
        assert_eq!(
            fx.reusable_prefix(),
            kept,
            "the kept part is a valid prefix"
        );

        fx.upload().unwrap();
        assert_eq!(fx.uploaded(), fx.data);
        assert!(!fx.temp_exists());
    }

    #[test]
    fn pump_keeps_to_the_speed_limit() {
        let data = vec![7u8; CHUNK_SIZE * 2];
        let control = UploadControl {
            cancel: &never,
            bytes_per_sec: Some(4 * CHUNK_SIZE as u64),
        };

        let started = Instant::now();
        let mut hasher = Sha256::new();
        pump(
            &mut &data[..],
            &mut Vec::new(),
            &mut hasher,
            0,
            &control,
            &mut |_| {},
        )
        .unwrap();
        // Two chunks at four chunks per second take half a second.
        assert!(started.elapsed() >= std::time::Duration::from_millis(450));
    }

    #[test]
    fn the_speed_limit_does_not_delay_a_stop() {
        let data = vec![7u8; CHUNK_SIZE * 2];
        let stop_at = Instant::now() + std::time::Duration::from_millis(100);
        let control = UploadControl {
            cancel: &|| Instant::now() >= stop_at,
            bytes_per_sec: Some(1),
        };

        let started = Instant::now();
        let mut hasher = Sha256::new();
        let error = pump(
            &mut &data[..],
            &mut Vec::new(),
            &mut hasher,
            0,
            &control,
            &mut |_| {},
        )
        .unwrap_err();
        assert!(error.is::<Cancelled>());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }
}
