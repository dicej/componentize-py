#![deny(warnings)]

use {
    anyhow::{Context, Error, Result},
    heck::ToSnakeCase,
    std::{
        collections::{hash_map::Entry, HashMap},
        env,
        fs::{self, File},
        io::{self, Cursor, Read, Seek},
        path::Path,
        rc::Rc,
        str,
        sync::Mutex,
    },
    summary::Summary,
    tar::Archive,
    wasi_common::WasiCtx,
    wasmtime::Linker,
    wasmtime_wasi::{sync::Dir, WasiCtxBuilder, WasiFile},
    wit_parser::{Resolve, UnresolvedPackage, WorldId},
    wizer::Wizer,
    zstd::Decoder,
};

mod abi;
mod bindgen;
pub mod command;
mod componentize;
mod convert;
#[cfg(feature = "pyo3")]
mod python;
mod summary;
#[cfg(test)]
mod test;
mod util;

wasmtime::bindgen!({
    world: "init",
    path: "wit/init.wit"
});

#[cfg(unix)]
const NATIVE_PATH_DELIMITER: char = ':';

#[cfg(windows)]
const NATIVE_PATH_DELIMITER: char = ';';

struct Ctx {
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

impl Invoker for MyInvoker {
    fn call_s32(&mut self, function: &str) -> Result<i32> {
        let func = instance
            .exports(&mut store)
            .root()
            .typed_func::<(), (i32,)>(function)?;
        func.call_async(&mut store, ()).await?.0
    }

    fn call_s64(&mut self, function: &str) -> Result<i64> {
        let func = instance
            .exports(&mut store)
            .root()
            .typed_func::<(), (i64,)>(function)?;
        func.call_async(&mut store, ()).await?.0
    }

    fn call_float32(&mut self, function: &str) -> Result<f32> {
        let func = instance
            .exports(&mut store)
            .root()
            .typed_func::<(), (f32,)>(function)?;
        func.call_async(&mut store, ()).await?.0
    }

    fn call_float64(&mut self, function: &str) -> Result<f64> {
        let func = instance
            .exports(&mut store)
            .root()
            .typed_func::<(), (f64,)>(function)?;
        func.call_async(&mut store, ()).await?.0
    }

    fn call_list_u8(&mut self, function: &str) -> Result<Vec<u8>> {
        let func = instance
            .exports(&mut store)
            .root()
            .typed_func::<(), (Vec<u8>,)>(function)?;
        func.call_async(&mut store, ()).await?.0
    }
}

fn open_dir(path: impl AsRef<Path>) -> Result<Dir> {
    Dir::open_ambient_dir(path, wasmtime_wasi::sync::ambient_authority()).map_err(Error::from)
}

fn file(file: File) -> Box<dyn WasiFile + 'static> {
    Box::new(wasmtime_wasi::file::File::from_cap_std(
        cap_std::fs::File::from_std(file),
    ))
}

pub fn generate_bindings(wit_path: &Path, world: Option<&str>, output_dir: &Path) -> Result<()> {
    let (resolve, world) = parse_wit(wit_path, world)?;
    let summary = Summary::try_new(&resolve, world)?;
    fs::create_dir_all(output_dir)?;
    summary.generate_code(output_dir)
}

pub fn componentize(
    wit_path: &Path,
    world: Option<&str>,
    python_path: &str,
    app_name: &str,
    stub_wasi: bool,
    output_path: &Path,
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

    let mut linker = Linker::default()
        .library(
            "componentize-py-runtime.so",
            &zstd::decode_all(Cursor::new(include_bytes!(concat!(
                env!("OUT_DIR"),
                "/componentize-py-runtime.so.zst"
            ))))?,
        )?
        .library(
            "libc.so",
            &zstd::decode_all(Cursor::new(include_bytes!(concat!(
                env!("OUT_DIR"),
                "/libc.so.zst"
            ))))?,
        )?
        .library(
            "libc++.so",
            &zstd::decode_all(Cursor::new(include_bytes!(concat!(
                env!("OUT_DIR"),
                "/libc++.so.zst"
            ))))?,
        )?
        .library(
            "libc++abi.so",
            &zstd::decode_all(Cursor::new(include_bytes!(concat!(
                env!("OUT_DIR"),
                "/libc++abi.so.zst"
            ))))?,
        )?
        .library(
            "componentize-py-bindings.so",
            make_bindings(&resolve, world, &summary),
        )?;

    if stub_wasi {
        linker = linker.library("componentize-py-wasi-stub.so", make_wasi_stub_library())?;
    } else {
        linker = linker.adapter(
            "wasi-snapshot-preview1",
            &zstd::decode_all(Cursor::new(include_bytes!(concat!(
                env!("OUT_DIR"),
                "/wasi_snapshot_preview1.wasm.zst"
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

    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    let stdin = tempfile::tempfile()?;

    let mut wasi = WasiCtxBuilder::new()
        .stdin(file(stdin))
        .stdout(file(stdout.try_clone()?))
        .stderr(file(stdout.try_clone()?))
        .env("PYTHONUNBUFFERED", "1")?
        .env("COMPONENTIZE_PY_APP_NAME", app_name)?
        .env("PYTHONHOME", "/python")?
        .preopened_dir(open_dir(stdlib.path())?, "python")?;

    let mut count = 0;
    for (index, path) in python_path.split(NATIVE_PATH_DELIMITER).enumerate() {
        wasi = wasi.preopened_dir(open_dir(path)?, &index.to_string())?;
        count += 1;
    }

    let python_path = (0..count)
        .map(|index| format!("/{index}"))
        .collect::<Vec<_>>()
        .join(":");

    let mut table = Table::new();
    let wasi = wasi
        .env("PYTHONPATH", &format!("/python:{python_path}"))?
        .build(&mut table);

    let mut config = Config::new();
    config.wasm_component_model(true);
    config.async_support(true);

    let engine = Engine::new(&config)?;

    let mut linker = Linker::new(&engine);
    command::add_to_linker(&mut linker)?;

    let component = component_init::initialize(&component, move |instrumented| {
        let (exports, instance) =
            Exports::instantiate_async(&mut store, &Component::new(&engine, instrumented)?).await?;

        exports.call_init(&mut store, symbols)??;

        Box::new(MyInvoker { store, instance })
    })
    .with_context(move || {
        let mut buffer = String::new();
        if stdout.rewind().is_ok() {
            _ = stdout.read_to_string(&mut buffer);
        }

        if stderr.rewind().is_ok() {
            _ = stderr.read_to_string(&mut buffer);
            _ = io::copy(&mut stderr, &mut io::stderr().lock());
        }

        buffer
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
        resolve.push(pkg, &Default::default())?
    };
    let world = resolve.select_world(pkg, world)?;
    Ok((resolve, world))
}

fn make_wasi_stub_code(name: &str) -> Vec<Ins> {
    // For most stubs, we trap, but we need specialized stubs for the functions called by `wasi-libc`'s
    // __wasm_call_ctors; otherwise we'd trap immediately upon calling any export.
    match name {
        "clock_time_get" => vec![
            // *time = 0;
            Ins::LocalGet(2),
            Ins::I64Const(0),
            Ins::I64Store(bindgen::mem_arg(0, 3)),
            // return ERRNO_SUCCESS;
            Ins::I32Const(0),
        ],
        "environ_sizes_get" => vec![
            // *environc = 0;
            Ins::LocalGet(0),
            Ins::I32Const(0),
            Ins::I32Store(bindgen::mem_arg(0, 2)),
            // *environ_buf_size = 0;
            Ins::LocalGet(1),
            Ins::I32Const(0),
            Ins::I32Store(bindgen::mem_arg(0, 2)),
            // return ERRNO_SUCCESS;
            Ins::I32Const(0),
        ],
        "fd_prestat_get" => vec![
            // return ERRNO_BADF;
            Ins::I32Const(8),
        ],
        _ => vec![Ins::Unreachable],
    }
}

fn make_wasi_stub_library() -> Vec<u8> {
    let signatures = [
        (
            "args_get",
            &[ValType::I32, ValType::I32] as &[_],
            ValType::I32,
        ),
        ("arg_sizes_get", &[ValType::I32, ValType::I32], ValType::I32),
    ];

    let mut types = TypeSection::new();
    let mut exports = ExportSection::new();
    let mut functions = FunctionSection::new();
    let mut code = CodeSection::new();
    for (offset, (name, params, result)) in signatures.iter().enumerate() {
        let offset = u32::try_from(offset).unwrap();
        types.function(params.iter().copied(), [*result]);
        functions.function(offset);
        let mut function = Function::new([]);
        function.instruction(&Ins::Unreachable);
        function.instruction(&Ins::End);
        code.function(&function);
        exports.export(name, ExportKind::Func, offset);
    }

    let mut module = Module::new();

    module.section(&types);
    module.section(&functions);
    module.section(&exports);
    module.section(&code);

    module.finish()
}
