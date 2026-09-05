// SPDX-License-Identifier: GPL-3.0-or-later
//! Generate the man page (`niribg.1`) and bash/zsh/fish completions into
//! `$OUT_DIR` at build time. `cargo install` does not install these; distro
//! packaging picks them out of `target/<profile>/build/niribg-*/out/`.

#[path = "src/cli.rs"]
mod cli;

use std::path::PathBuf;

use clap_complete::Shell;

fn main() {
    println!("cargo::rerun-if-changed=src/cli.rs");

    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR set by cargo"));
    let mut cmd = cli::command();

    let mut man_page = Vec::new();
    clap_mangen::Man::new(cmd.clone())
        .render(&mut man_page)
        .expect("rendering the man page");
    std::fs::write(out.join("niribg.1"), man_page).expect("writing niribg.1");

    for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
        clap_complete::generate_to(shell, &mut cmd, "niribg", &out)
            .expect("generating shell completions");
    }
}
