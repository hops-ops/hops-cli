use super::backend::Backend;
use clap::Args;
use std::error::Error;
use std::io::{self, Write};

#[derive(Args, Debug)]
pub struct UninstallArgs {
    /// Uninstall the backend binary even if its local cluster still exists.
    #[arg(long)]
    pub force: bool,
}

pub fn run(backend: Backend, args: &UninstallArgs) -> Result<(), Box<dyn Error>> {
    run_inner(
        backend,
        args.force,
        Backend::cluster_exists,
        Backend::uninstall,
        super::backend::clear_persisted,
        confirm_uninstall,
    )
}

fn run_inner<ClusterExists, Uninstall, ClearPersisted, Confirm>(
    backend: Backend,
    force: bool,
    cluster_exists: ClusterExists,
    uninstall: Uninstall,
    clear_persisted: ClearPersisted,
    confirm: Confirm,
) -> Result<(), Box<dyn Error>>
where
    ClusterExists: FnOnce(Backend) -> bool,
    Uninstall: FnOnce(Backend) -> Result<(), Box<dyn Error>>,
    ClearPersisted: FnOnce() -> Result<(), Box<dyn Error>>,
    Confirm: FnOnce(Backend) -> Result<bool, Box<dyn Error>>,
{
    if !force && cluster_exists(backend) {
        return Err("destroy the cluster first: `hops local destroy` (or pass --force to uninstall the backend binary anyway)".into());
    }

    if confirm(backend)? {
        uninstall(backend)?;
        clear_persisted()?;
    } else {
        log::info!("Uninstall cancelled");
    }

    Ok(())
}

fn confirm_uninstall(backend: Backend) -> Result<bool, Box<dyn Error>> {
    print!(
        "Uninstall {}? This will remove the binary. [y/N] ",
        backend.name()
    );
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    Ok(input.trim().eq_ignore_ascii_case("y"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn refuses_uninstall_when_cluster_exists_without_force() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let err = run_inner(
            Backend::Kind,
            false,
            {
                let events = Rc::clone(&events);
                move |_| {
                    events.borrow_mut().push("cluster_exists");
                    true
                }
            },
            |_| {
                events.borrow_mut().push("uninstall");
                Ok(())
            },
            || {
                events.borrow_mut().push("clear");
                Ok(())
            },
            |_| {
                events.borrow_mut().push("confirm");
                Ok(true)
            },
        )
        .expect_err("cluster guard should refuse uninstall");

        assert!(err.to_string().contains("destroy the cluster first"));
        assert_eq!(&*events.borrow(), &["cluster_exists"]);
    }

    #[test]
    fn force_uninstalls_without_cluster_probe_and_clears_after_success() {
        let events = Rc::new(RefCell::new(Vec::new()));
        run_inner(
            Backend::Kind,
            true,
            {
                let events = Rc::clone(&events);
                move |_| {
                    events.borrow_mut().push("cluster_exists");
                    true
                }
            },
            {
                let events = Rc::clone(&events);
                move |_| {
                    events.borrow_mut().push("uninstall");
                    Ok(())
                }
            },
            {
                let events = Rc::clone(&events);
                move || {
                    events.borrow_mut().push("clear");
                    Ok(())
                }
            },
            {
                let events = Rc::clone(&events);
                move |_| {
                    events.borrow_mut().push("confirm");
                    Ok(true)
                }
            },
        )
        .expect("force should allow uninstall");

        assert_eq!(&*events.borrow(), &["confirm", "uninstall", "clear"]);
    }

    #[test]
    fn failed_uninstall_does_not_clear_persisted_backend() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let err = run_inner(
            Backend::Kind,
            false,
            |_| false,
            {
                let events = Rc::clone(&events);
                move |_| {
                    events.borrow_mut().push("uninstall");
                    Err("brew failed".into())
                }
            },
            {
                let events = Rc::clone(&events);
                move || {
                    events.borrow_mut().push("clear");
                    Ok(())
                }
            },
            |_| Ok(true),
        )
        .expect_err("uninstall failure should propagate");

        assert_eq!(err.to_string(), "brew failed");
        assert_eq!(&*events.borrow(), &["uninstall"]);
    }
}
