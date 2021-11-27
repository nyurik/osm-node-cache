extern crate rustc_version;

use std::env;
use std::fs::OpenOptions;
use rustc_version::{version_meta, Channel};
use std::io::Write;

fn main() {
    let meta = version_meta().unwrap();
    let mut file = OpenOptions::new().write(true).create(true).open("./build.txt").unwrap();
    // writeln!(file, "{:#?}", meta).unwrap();
    for (key, value) in env::vars() {
        writeln!(file, "{}: {}", key, value).unwrap();
    }
    writeln!(file, "\n\n\n").unwrap();
    if meta.channel == Channel::Nightly {
        println!("cargo:rustc-cfg=feature=\"nightly\"");
    }
}
