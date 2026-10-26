fn main() {
    // На станции библиотеки вендорских ALSA-плагинов лежат в этом каталоге и подгружаются
    // транзитивно, поэтому нужен именно RPATH (а не RUNPATH).
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("arm") {
        println!("cargo:rustc-link-arg=-Wl,--disable-new-dtags,-rpath,/system/vendor/quasar/libs");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
