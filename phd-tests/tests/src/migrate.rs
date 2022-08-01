use std::time::Duration;

use phd_testcase::*;

#[phd_testcase]
fn smoke_test(ctx: &TestContext) {
    let mut source = ctx
        .vm_factory
        .new_vm("migration_smoke_source", ctx.vm_factory.default_vm_config())?;

    source.launch()?;
    source.wait_to_boot()?;
    let lsout = source.run_shell_command("ls foo.bar 2> /dev/null")?;
    assert_eq!(lsout, "");
    source.run_shell_command("touch foo.bar")?;

    let mut target = ctx
        .vm_factory
        .new_vm("migration_smoke_target", ctx.vm_factory.default_vm_config())?;

    target.migrate_from(&source, Duration::from_secs(60))?;
    let lsout = target.run_shell_command("ls foo.bar")?;
    assert_eq!(lsout, "foo.bar");
}
