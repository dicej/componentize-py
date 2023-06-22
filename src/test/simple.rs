#![deny(warnings)]

use {
    super::{Ctx, ENGINE},
    anyhow::Result,
    async_trait::async_trait,
    wasmtime::{
        component::{Component, Linker},
        Store,
    },
    wasmtime_wasi::preview2::{wasi, Table, WasiCtxBuilder},
};

#[tokio::test]
async fn simple_export() -> Result<()> {
    wasmtime::component::bindgen!({
        path: "src/test/wit",
        world: "simple-export-test",
        async: true
    });

    let component = &super::make_component(
        include_str!("wit/simple-export.wit"),
        r#"
from simple_export_test import exports

class SimpleExport(exports.SimpleExport):
    def foo(v: int) -> int:
        return v + 3
"#,
    )
    .await?;

    let mut linker = Linker::new(&ENGINE);
    wasi::command::add_to_linker(&mut linker)?;

    let mut table = Table::new();
    let wasi = WasiCtxBuilder::new()
        .inherit_stdout()
        .inherit_stderr()
        .build(&mut table)?;

    let mut store = Store::new(&ENGINE, Ctx { wasi, table });

    let (instance, _) = SimpleExportTest::instantiate_async(
        &mut store,
        &Component::new(&ENGINE, component)?,
        &linker,
    )
    .await?;

    assert_eq!(
        45,
        instance
            .componentize_py_test_simple_export()
            .call_foo(&mut store, 42)
            .await?
    );

    Ok(())
}

#[tokio::test]
async fn simple_import_and_export() -> Result<()> {
    wasmtime::component::bindgen!({
        path: "src/test/wit",
        world: "simple-import-and-export-test",
        async: true
    });

    #[async_trait]
    impl componentize_py::test::simple_import_and_export::Host for Ctx {
        async fn foo(&mut self, v: u32) -> Result<u32> {
            Ok(v + 2)
        }
    }

    let component = &super::make_component(
        include_str!("wit/simple-import-and-export.wit"),
        r#"
from simple_import_and_export import exports
from simple_import_and_export.imports import imports

class Exports(exports.Exports):
    def foo(v: int) -> int:
        return imports.foo(v) + 3
"#,
    )
    .await?;

    let mut linker = Linker::<Ctx>::new(&ENGINE);
    wasi::command::add_to_linker(&mut linker)?;
    componentize_py::test::simple_import_and_export::add_to_linker(&mut linker, |ctx| ctx)?;

    let mut table = Table::new();
    let wasi = WasiCtxBuilder::new()
        .inherit_stdout()
        .inherit_stderr()
        .build(&mut table)?;

    let mut store = Store::new(&ENGINE, Ctx { wasi, table });

    let (instance, _) = SimpleImportAndExportTest::instantiate_async(
        &mut store,
        &Component::new(&ENGINE, component)?,
        &linker,
    )
    .await?;

    assert_eq!(
        47,
        instance
            .componentize_py_test_simple_import_and_export()
            .call_foo(&mut store, 42)
            .await?
    );

    Ok(())
}
