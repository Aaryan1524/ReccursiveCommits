use std::{env, path::PathBuf, process::ExitCode};

use reccursive_daemon::{LocalService, ServicePaths};

fn main() -> ExitCode {
    let mut arguments = env::args_os().skip(1);
    let Some(flag) = arguments.next() else {
        eprintln!("usage: reccursive-daemon --state-dir <path>");
        return ExitCode::from(2);
    };
    let Some(path) = arguments.next() else {
        eprintln!("usage: reccursive-daemon --state-dir <path>");
        return ExitCode::from(2);
    };
    if flag != "--state-dir" || arguments.next().is_some() {
        eprintln!("usage: reccursive-daemon --state-dir <path>");
        return ExitCode::from(2);
    }

    match LocalService::bind(ServicePaths::new(PathBuf::from(path)))
        .and_then(|it| it.serve_forever())
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("service failed: {error}");
            ExitCode::from(1)
        }
    }
}
