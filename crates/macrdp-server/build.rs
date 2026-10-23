fn main() {
    // libswift_Concurrency ships in the dyld shared cache on macOS 12+ (we require 14+), so it is
    // never on disk under /usr/lib/swift. Point @rpath there; an rpath to Xcode's swift-5.5
    // back-deploy copy loads a second Swift runtime ("Class ... is implemented in both").
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
}
