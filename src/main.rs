use anyhow::{Context, Result, anyhow, bail};
use futures::lock::Mutex;
use nvim_rs::{Neovim, compat::tokio::Compat, create::tokio as create};
use once_cell::sync::OnceCell;
use rmpv::Value;
use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::os::unix::process::CommandExt;
use std::process::{Command, exit};
use std::sync::Arc;
use tokio::{io::WriteHalf, net::UnixStream};

// This is nvim's vim_strsave_fnameescape for VSE_NONE on non-Windows, with all the other control
// flow folded out. This avoids an RPC round-trip for each file edit, but could conceivably drift
// from nvim's definition.
fn fnameescape<S: AsRef<str>>(s: S) -> String {
    let s = s.as_ref();
    static PATH_ESC_CHARS: OnceCell<HashSet<char>> = OnceCell::new();
    let path_esc_chars = PATH_ESC_CHARS.get_or_init(|| {
        let mut h = HashSet::new();
        for c in " \t\n*?[{`$\\%#'\"|!<".chars() {
            h.insert(c);
        }
        h
    });
    let mut escaped = String::with_capacity(s.len());
    if s.starts_with('>') || s.starts_with('+') {
        escaped.push('\\');
    }
    for c in s.chars() {
        if path_esc_chars.contains(&c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

struct State {
    /// Whether there was any error. This is rather coarse-grained, and we don't even attempt to
    /// retain the actual error code, but really this is just so the user's shell sees failures so
    /// that is good enough.
    error: bool,
    /// How many buffers we are waiting on.
    buffers: usize,
}

#[derive(Clone)]
struct Handler {
    state: Arc<Mutex<State>>,
}

impl Handler {
    pub fn new(buffers: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                error: false,
                buffers,
            })),
        }
    }
}

impl nvim_rs::Handler for Handler {
    type Writer = Compat<WriteHalf<UnixStream>>;

    async fn handle_notify(&self, name: String, args: Vec<Value>, _nvim: Neovim<Self::Writer>) {
        let name = name.as_str();
        let is_exit = name == "Exit";
        if !is_exit && name != "BufDelete" {
            return;
        }
        let exitcode: u64 = args
            .first()
            .unwrap_or(&Value::from(if is_exit { 1 } else { 0 }))
            .as_ref()
            .try_into()
            .expect("invalid notification arg");
        let mut state = self.state.lock().await;
        if exitcode != 0 {
            state.error = true;
        }
        state.buffers -= 1;
        if is_exit || state.buffers == 0 {
            exit(if state.error { 1 } else { 0 });
        }
    }
}

fn exec_nvim<I, S>(args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Result::Err(Command::new("nvim").args(args).exec().into())
}

async fn remote_edit<P, S, I>(path: P, args: I, wait: bool) -> Result<()>
where
    P: AsRef<OsStr>,
    S: AsRef<OsStr>,
    I: IntoIterator<Item = S>,
{
    let args = args.into_iter();
    let mut escaped_names = vec![];
    for arg in args {
        escaped_names.push(fnameescape(
            arg.as_ref().to_str().context("non-UTF8 filename")?,
        ));
    }
    let handler = Handler::new(escaped_names.len());
    let (nvim, io) = create::new_path(path.as_ref(), handler).await?;
    let api_info = nvim
        .get_api_info()
        .await
        .context("could not get API info")?;
    let channel_id: u64 = api_info
        .first()
        .context("empty API info")?
        .as_ref()
        .try_into()
        .map_err(|_| anyhow!("invalid API info channel"))?;
    for escaped_name in escaped_names {
        nvim.command(format!("edit {escaped_name}").as_str())
            .await?;
        nvim.command("augroup vclosenotif").await?;
        nvim.command(
            format!(
                "autocmd BufDelete <buffer> silent! call rpcnotify({channel_id}, \"BufDelete\")"
            )
            .as_str(),
        )
        .await?;
        nvim.command(format!("autocmd VimLeave * if exists(\"v:exiting\") && v:exiting > 0 | silent! call rpcnotify({channel_id}, \"Exit\", v:exiting) | endif").as_str()).await?;
        nvim.command("augroup END").await?;
    }
    if !wait {
        return Ok(());
    }
    Ok(io.await??)
}

#[tokio::main]
async fn main() -> Result<()> {
    let socket_path = env::var_os("NVIM");
    let mut args = env::args_os().skip(1).peekable();
    let wait = args
        .next_if(|arg| arg.as_os_str() == OsStr::new("--wait"))
        .is_some();
    match (args.peek(), socket_path) {
        (None, None) => exec_nvim(["+term", "+starti"]),
        (None, Some(_)) => bail!("refusing to nest"),
        (Some(_), None) => exec_nvim(args),
        (Some(_), Some(path)) => remote_edit(path, args, wait).await,
    }
}
