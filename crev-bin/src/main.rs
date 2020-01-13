#![allow(deprecated)]

#[macro_use]
extern crate quicli;
use crev_common;

use rprompt;
#[macro_use]
extern crate structopt;

use crev_lib::{local::Local, repo::Repo};
use default::default;
use hex;
use std::path::PathBuf;
use structopt::StructOpt;

mod opts;
mod util;

main!(|opts: opts::Opts| match opts.command {
    opts::Command::Id(id) => match id.id_command {
        opts::IdCommand::Show => unimplemented!(),
        opts::IdCommand::New => unimplemented!(),
    },
    opts::Command::Trust(trust) => match trust {
        opts::Trust::Add(_trust) => unimplemented!(),
    },
    opts::Command::Add(add) => {
        let mut repo = Repo::auto_open()?;
        repo.add(add.paths)?;
    }
    opts::Command::Commit(opts) => {
        let mut repo = Repo::auto_open()?;
        if opts.all {
        } else {
            repo.commit(&crev_common::read_passphrase, opts.allow_dirty)?;
        }
    }
    opts::Command::Package(package) => match package {
        opts::Package::Init => {
            let local = Local::auto_open()?;
            let cur_id = local.read_current_id()?;
            Repo::init(&PathBuf::from(".".to_string()), cur_id.to_string())?;
        }
        opts::Package::Trust(package_trust) => {
            let mut repo = Repo::auto_open()?;
            repo.trust_package(&crev_common::read_passphrase, package_trust.allow_dirty)?;
        }
        opts::Package::Verify(args) => {
            let mut repo = Repo::auto_open()?;
            let local = Local::auto_create_or_open()?;
            println!(
                "{}",
                repo.package_verify(&local, args.allow_dirty, args.for_id, &args.trust_params)?
            );
        }
        opts::Package::Digest(digest) => {
            let mut repo = Repo::auto_open()?;
            println!("{}", repo.package_digest(digest.allow_dirty)?);
        }
    },
    opts::Command::Status => {
        let mut repo = Repo::auto_open()?;
        repo.status()?;
    }
    opts::Command::Remove(remove) => {
        let mut repo = Repo::auto_open()?;
        repo.remove(remove.paths)?;
    }
    opts::Command::Verify(verify_opts) => {
        let mut repo = Repo::auto_open()?;
        repo.package_verify(verify_opts.allow_dirty)?;
    }
    opts::Command::Db(cmd) => match cmd {
        opts::Db::Git(git) => {
            let local = Local::auto_open()?;
            let status = local.run_git(git.args)?;
            std::process::exit(status.code().unwrap_or(-159));
        }
        opts::Db::Fetch => {
            let local = Local::auto_open()?;
            local.fetch_trusted(default())?;
        }
    },
});
