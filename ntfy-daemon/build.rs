fn main() {
    capnpc::CompilerCommand::new()
        .file("src/ntfy.capnp")
        .run()
        .unwrap();
}
