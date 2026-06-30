fn main() {
    println!("cargo:rerun-if-env-changed=NYATERM_GITHUB_GIST_CLIENT_ID");

    if let Ok(client_id) = std::env::var("NYATERM_GITHUB_GIST_CLIENT_ID") {
        println!("cargo:rustc-env=NYATERM_GITHUB_GIST_CLIENT_ID={client_id}");
    }

    tauri_build::build();
}
