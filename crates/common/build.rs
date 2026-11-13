//! Compiles the Devocub Antichatter port's tests in when the port is present
//! (it is kept out of the public project for licensing reasons).

fn main() {
    let port = "../../plugins/devocub-antichatter/tests.rs";
    println!("cargo::rustc-check-cfg=cfg(kt_antichatter)");
    println!("cargo::rerun-if-changed={port}");
    if std::path::Path::new(port).exists() {
        println!("cargo::rustc-cfg=kt_antichatter");
    }
}
