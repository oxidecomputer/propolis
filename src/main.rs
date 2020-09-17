mod vm;

extern crate bhyve_api;

fn main() {

    let vm = vm::create_vm("testvm").unwrap();

    println!("vm created");
    drop(vm);
}
