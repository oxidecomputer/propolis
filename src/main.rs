mod vm;

extern crate bhyve_api;

fn main() {

    let mut vm = vm::create_vm("testvm").unwrap();

    println!("vm created");
    let _ = vm.setup_memory(512 * 1024 * 1024);
    drop(vm);
}
