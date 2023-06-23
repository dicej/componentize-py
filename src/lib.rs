#![deny(warnings)]

use {
    anyhow::{anyhow, Context, Result},
    async_trait::async_trait,
    cap_std::fs::Dir,
    component_init::Invoker,
    exports::exports::RawUnionType,
    futures::future::FutureExt,
    heck::ToSnakeCase,
    std::{
        env,
        fs::{self, File},
        hash::{Hash, Hasher},
        io::Cursor,
        mem,
        path::Path,
        str,
    },
    summary::Summary,
    tar::Archive,
    wasmtime::{
        component::{Component, Instance, Linker},
        Config, Engine, Store,
    },
    wasmtime_wasi::preview2::{
        pipe::{ReadPipe, WritePipe},
        DirPerms, FilePerms, Table, WasiCtx, WasiCtxBuilder, WasiView,
    },
    wit_parser::{Resolve, UnresolvedPackage, WorldId},
    zstd::Decoder,
};

mod abi;
mod bindgen;
mod bindings;
pub mod command;
#[cfg(feature = "pyo3")]
mod python;
mod summary;
#[cfg(test)]
mod test;
mod util;

wasmtime::component::bindgen!({
    path: "wit/init.wit",
    world: "init",
    async: true
});

impl Hash for RawUnionType {
    fn hash<H: Hasher>(&self, state: &mut H) {
        mem::discriminant(self).hash(state)
    }
}

#[cfg(unix)]
const NATIVE_PATH_DELIMITER: char = ':';

#[cfg(windows)]
const NATIVE_PATH_DELIMITER: char = ';';

pub struct Ctx {
    wasi: WasiCtx,
    table: Table,
}

impl WasiView for Ctx {
    fn ctx(&self) -> &WasiCtx {
        &self.wasi
    }
    fn ctx_mut(&mut self) -> &mut WasiCtx {
        &mut self.wasi
    }
    fn table(&self) -> &Table {
        &self.table
    }
    fn table_mut(&mut self) -> &mut Table {
        &mut self.table
    }
}

struct MyInvoker {
    store: Store<Ctx>,
    instance: Instance,
}

#[async_trait]
impl Invoker for MyInvoker {
    async fn call_s32(&mut self, function: &str) -> Result<i32> {
        let func = self
            .instance
            .exports(&mut self.store)
            .root()
            .typed_func::<(), (i32,)>(function)?;
        let result = func.call_async(&mut self.store, ()).await?.0;
        func.post_return_async(&mut self.store).await?;
        Ok(result)
    }

    async fn call_s64(&mut self, function: &str) -> Result<i64> {
        let func = self
            .instance
            .exports(&mut self.store)
            .root()
            .typed_func::<(), (i64,)>(function)?;
        let result = func.call_async(&mut self.store, ()).await?.0;
        func.post_return_async(&mut self.store).await?;
        Ok(result)
    }

    async fn call_float32(&mut self, function: &str) -> Result<f32> {
        let func = self
            .instance
            .exports(&mut self.store)
            .root()
            .typed_func::<(), (f32,)>(function)?;
        let result = func.call_async(&mut self.store, ()).await?.0;
        func.post_return_async(&mut self.store).await?;
        Ok(result)
    }

    async fn call_float64(&mut self, function: &str) -> Result<f64> {
        let func = self
            .instance
            .exports(&mut self.store)
            .root()
            .typed_func::<(), (f64,)>(function)?;
        let result = func.call_async(&mut self.store, ()).await?.0;
        func.post_return_async(&mut self.store).await?;
        Ok(result)
    }

    async fn call_list_u8(&mut self, function: &str) -> Result<Vec<u8>> {
        let func = self
            .instance
            .exports(&mut self.store)
            .root()
            .typed_func::<(), (Vec<u8>,)>(function)?;
        let result = func.call_async(&mut self.store, ()).await?.0;
        func.post_return_async(&mut self.store).await?;
        Ok(result)
    }
}

pub fn generate_bindings(wit_path: &Path, world: Option<&str>, output_dir: &Path) -> Result<()> {
    let (resolve, world) = parse_wit(wit_path, world)?;
    let summary = Summary::try_new(&resolve, world)?;
    fs::create_dir_all(output_dir)?;
    summary.generate_code(output_dir)
}

pub async fn componentize(
    wit_path: &Path,
    world: Option<&str>,
    python_path: &str,
    app_name: &str,
    stub_wasi: bool,
    output_path: &Path,
    add_to_linker: &dyn Fn(&mut Linker<Ctx>) -> Result<()>,
) -> Result<()> {
    let stdlib = tempfile::tempdir()?;

    Archive::new(Decoder::new(Cursor::new(include_bytes!(concat!(
        env!("OUT_DIR"),
        "/python-lib.tar.zst"
    ))))?)
    .unpack(stdlib.path())?;

    let (resolve, world) = parse_wit(wit_path, world)?;
    let summary = Summary::try_new(&resolve, world)?;
    let symbols = summary.collect_symbols();

    let mut linker = wit_component::Linker::default()
        .validate(true)
        .library(
            "libcomponentize_py_runtime.so",
            &zstd::decode_all(Cursor::new(include_bytes!(concat!(
                env!("OUT_DIR"),
                "/libcomponentize_py_runtime.so.zst"
            ))))?,
            false,
        )?
        .library(
            "libpython3.11.so",
            &zstd::decode_all(Cursor::new(include_bytes!(concat!(
                env!("OUT_DIR"),
                "/libpython3.11.so.zst"
            ))))?,
            false,
        )?
        .library(
            "libc.so",
            &zstd::decode_all(Cursor::new(include_bytes!(concat!(
                env!("OUT_DIR"),
                "/libc.so.zst"
            ))))?,
            false,
        )?
        .library(
            "libc++.so",
            &zstd::decode_all(Cursor::new(include_bytes!(concat!(
                env!("OUT_DIR"),
                "/libc++.so.zst"
            ))))?,
            false,
        )?
        .library(
            "libc++abi.so",
            &zstd::decode_all(Cursor::new(include_bytes!(concat!(
                env!("OUT_DIR"),
                "/libc++abi.so.zst"
            ))))?,
            false,
        )?
        .library(
            "libcomponentize_py_bindings.so",
            &bindings::make_bindings(&resolve, world, &summary)?,
            false,
        )?;

    if stub_wasi {
        linker = linker.library(
            "libcomponentize_py_wasi_stub.so",
            &bindings::make_wasi_stub_library(),
            false,
        )?;
    } else {
        linker = linker.adapter(
            "wasi_snapshot_preview1",
            &zstd::decode_all(Cursor::new(include_bytes!(concat!(
                env!("OUT_DIR"),
                "/wasi_preview1_component_adapter.wasm.zst"
            ))))?,
        )?;
    }

    // todo: add `--dl-openable` options for any .cpython-311-wasm32-wasi.so files found in `python_path`

    let component = linker.encode()?;

    let generated_code = tempfile::tempdir()?;
    let world_dir = generated_code
        .path()
        .join(resolve.worlds[world].name.to_snake_case());
    fs::create_dir_all(&world_dir)?;
    summary.generate_code(&world_dir)?;

    let python_path = format!(
        "{python_path}{NATIVE_PATH_DELIMITER}{}",
        generated_code
            .path()
            .to_str()
            .context("non-UTF-8 temporary directory name")?
    );

    let stdout = WritePipe::new_in_memory();
    let stderr = WritePipe::new_in_memory();

    let mut wasi = WasiCtxBuilder::new()
        .set_stdin(ReadPipe::from(""))
        .set_stdout(stdout.clone())
        .set_stderr(stderr.clone())
        .push_env("PYTHONUNBUFFERED", "1")
        .push_env("COMPONENTIZE_PY_APP_NAME", app_name)
        .push_env("PYTHONHOME", "/python")
        .push_preopened_dir(
            Dir::from_std_file(File::open(stdlib.path())?),
            DirPerms::all(),
            FilePerms::all(),
            "python",
        );

    let mut count = 0;
    for (index, path) in python_path.split(NATIVE_PATH_DELIMITER).enumerate() {
        wasi = wasi.push_preopened_dir(
            Dir::from_std_file(File::open(path)?),
            DirPerms::all(),
            FilePerms::all(),
            &index.to_string(),
        );
        count += 1;
    }

    let python_path = (0..count)
        .map(|index| format!("/{index}"))
        .collect::<Vec<_>>()
        .join(":");

    let mut table = Table::new();
    let wasi = wasi
        .push_env("PYTHONPATH", format!("/python:{python_path}"))
        .build(&mut table)?;

    let mut config = Config::new();
    config.wasm_component_model(true);
    config.async_support(true);

    let engine = Engine::new(&config)?;

    let mut linker = Linker::new(&engine);
    add_to_linker(&mut linker)?;

    let mut store = Store::new(&engine, Ctx { wasi, table });

    let app_name = app_name.to_owned();
    let component = component_init::initialize(&component, move |instrumented| {
        async move {
            let (init, instance) = Init::instantiate_async(
                &mut store,
                &Component::new(&engine, instrumented)?,
                &linker,
            )
            .await?;

            init.exports()
                .call_init(&mut store, &app_name, &symbols)
                .await?
                .map_err(|e| anyhow!("{e}"))?;

            Ok(Box::new(MyInvoker { store, instance }) as Box<dyn Invoker>)
        }
        .boxed()
    })
    .await
    .with_context(move || {
        format!(
            "{}{}",
            String::from_utf8_lossy(&stdout.try_into_inner().unwrap().into_inner()),
            String::from_utf8_lossy(&stderr.try_into_inner().unwrap().into_inner())
        )
    })?;

    fs::write(output_path, component)?;

    Ok(())
}

fn parse_wit(path: &Path, world: Option<&str>) -> Result<(Resolve, WorldId)> {
    let mut resolve = Resolve::default();
    let pkg = if path.is_dir() {
        resolve.push_dir(path)?.0
    } else {
        let pkg = UnresolvedPackage::parse_file(path)?;
        resolve.push(pkg)?
    };
    let world = resolve.select_world(pkg, world)?;
    Ok((resolve, world))
}
