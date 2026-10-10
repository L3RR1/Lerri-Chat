fn main() {
    // Só o .exe do Windows leva o ícone como recurso.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=lerrichat.rc");
        println!("cargo:rerun-if-changed=assets/lerrichat.ico");
        embed_resource::compile("lerrichat.rc", embed_resource::NONE).manifest_required().expect("ícone do .exe");
    }
}
