use {
    crate::{
        bindgen::{self, FunctionBindgen, DISPATCH_CORE_PARAM_COUNT, LINK_LIST},
        convert::{
            self, IntoEntityType, IntoExportKind, IntoRefType, IntoTableType, IntoValType,
            MyElements,
        },
        summary::{FunctionKind, Summary},
    },
    anyhow::{bail, Result},
    indexmap::IndexSet,
    std::{cmp::Ordering, collections::HashMap, env, io::Cursor},
    wasm_encoder::{
        CodeSection, ConstExpr, CustomSection, ElementSection, Elements, Encode, EntityType,
        ExportKind, ExportSection, Function, FunctionSection, HeapType, ImportSection,
        Instruction as Ins, Module, RawSection, RefType, TableSection, TableType, TypeSection,
        ValType,
    },
    wit_component::{metadata, ComponentEncoder},
    wit_parser::{Resolve, WorldId},
};

pub fn make_bindings(resolve: &Resolve, world: WorldId, summary: &Summary) -> Result<Vec<u8>> {
    let import_signatures = [
        ("componentize_py#Dispatch", [ValType::I32; 7], []),
        (
            "componentize_py#Allocate",
            [ValType::I32; 2],
            [ValType::I32],
        ),
        ("componentize_py#Free", [ValType::I32; 3], []),
        (
            "componentize_py#LowerI32",
            [ValType::I32; 2],
            [ValType::I32],
        ),
        (
            "componentize_py#LowerI64",
            [ValType::I32; 2],
            [ValType::I64],
        ),
        (
            "componentize_py#LowerF32",
            [ValType::I32; 2],
            [ValType::F32],
        ),
        (
            "componentize_py#LowerF64",
            [ValType::I32; 2],
            [ValType::F64],
        ),
        (
            "componentize_py#LowerChar",
            [ValType::I32; 2],
            [ValType::I32],
        ),
        ("componentize_py#LowerString", [ValType::I32; 3], []),
        (
            "componentize_py#GetField",
            [ValType::I32; 4],
            [ValType::I32],
        ),
        (
            "componentize_py#GetListLength",
            [ValType::I32; 2],
            [ValType::I32],
        ),
        (
            "componentize_py#GetListElement",
            [ValType::I32; 3],
            [ValType::I32],
        ),
        ("componentize_py#LiftI32", [ValType::I32; 2], [ValType::I32]),
        (
            "componentize_py#LiftI64",
            [ValType::I32, ValType::I64],
            [ValType::I32],
        ),
        (
            "componentize_py#LiftF32",
            [ValType::I32, ValType::F32],
            [ValType::I32],
        ),
        (
            "componentize_py#LiftF64",
            [ValType::I32, ValType::F64],
            [ValType::I32],
        ),
        (
            "componentize_py#LiftChar",
            [ValType::I32; 2],
            [ValType::I32],
        ),
        (
            "componentize_py#LiftString",
            [ValType::I32; 3],
            [ValType::I32],
        ),
        ("componentize_py#MakeList", [ValType::I32], [ValType::I32]),
        ("componentize_py#ListAppend", [ValType::I32; 3], []),
        ("componentize_py#None", [ValType::I32], [ValType::I32]),
        ("componentize_py#GetBytes", [ValType::I32; 4], []),
        (
            "componentize_py#MakeBytes",
            [ValType::I32; 3],
            [ValType::I32],
        ),
    ];

    // TODO: deduplicate types
    let mut types = TypeSection::new();
    let mut imports = ImportSection::new();
    let mut functions = FunctionSection::new();
    let mut exports = ExportSection::new();
    let mut code = CodeSection::new();
    let mut function_names = Vec::new();
    let mut global_names = Vec::new();

    for (name, params, results) in import_signatures {
        let offset = types.len().try_into().unwrap();
        types.function(params, results);
        imports.import("env", name, EntityType::Function(offset));
        function_names.push((offset, name));
    }

    for function in summary
        .functions
        .iter()
        .filter(|f| matches!(f.kind, FunctionKind::Import))
    {
        let (params, results) = function.core_import_type(resolve);
        let offset = types.len().try_into().unwrap();
        types.function(params, results);
        imports.import(
            function
                .interface
                .map(|i| i.name)
                .unwrap_or(&resolve.worlds[world].name),
            function.name,
            EntityType::Function(offset),
        );
        function_names.push((offset, format!("{}-import", function.internal_name())));
    }

    let import_function_count = imports.len();

    let table_base = 0;
    imports.import(
        "env",
        "__table_base",
        EntityType::Global(GlobalType {
            val_type: ValType::I32,
            mutable: false,
        }),
    );

    let stack_pointer = 1;
    imports.import(
        "env",
        "__stack_pointer",
        EntityType::Global(GlobalType {
            val_type: ValType::I32,
            mutable: true,
        }),
    );

    imports.import(
        "env",
        "memory",
        EntityType::Memory(MemoryType {
            minimum: 0,
            maximum: None,
            memory64: false,
            shared: false,
        }),
    );

    imports.import(
        "env",
        "__indirect_function_table",
        EntityType::Table(TableType {
            element_type: RefType::Func,
            minimum: summary
                .functions
                .filter(|function| function.is_dispatchable())
                .count(),
            maximum: None,
        }),
    );

    for function in &summary.functions {
        let offset = types.len().try_into().unwrap();
        let (params, results) = function.core_export_type(resolve);
        types.function(params, results);
        functions.function(offset);
        function_names.push((offset, function.internal_name()));
        let mut gen = FunctionBindgen::new(summary, function, stack_pointer_index, &link_map);

        match function.kind {
            FunctionKind::Import => {
                gen.compile_import(import_index.try_into().unwrap());
                import_index += 1;
            }
            FunctionKind::Export => gen.compile_export(
                exports
                    .get_index_of(&(function.interface.map(|i| i.name), function.name))
                    .unwrap()
                    .try_into()?,
                // next two `dispatch_index`es should be the lift and lower functions (see ordering
                // in `Summary::visit_function`):
                dispatch_index,
                dispatch_index + 1,
            ),
            FunctionKind::ExportLift => gen.compile_export_lift(),
            FunctionKind::ExportLower => gen.compile_export_lower(),
            FunctionKind::ExportPostReturn => gen.compile_export_post_return(),
        };

        let mut func = Function::new_with_locals_types(gen.local_types);
        for instruction in &gen.instructions {
            func.instruction(instruction);
        }
        func.instruction(&Ins::End);
        code.function(&func);

        if function.is_dispatchable() {
            dispatch_index += 1;
        }

        match function.kind {
            FunctionKind::Export | FunctionKind::ExportPostReturn => {
                exports.export(
                    &format!(
                        "{}{}",
                        if let FunctionKind::ExportPostReturn = function.kind {
                            "cabi_post_"
                        } else {
                            ""
                        },
                        if let Some(interface) = function.interface {
                            format!("{}#{}", interface.name, function.name)
                        } else {
                            function.name.to_owned()
                        }
                    ),
                    ExportKind::Func,
                    (old_function_count + new_import_count + index)
                        .try_into()
                        .unwrap(),
                );
            }

            _ => (),
        }
    }

    {
        // dispatch export
        let offset = types.len().try_into().unwrap();
        types.function([ValType::I32; DISPATCH_CORE_PARAM_COUNT], []);
        let mut dispatch = Function::new([]);

        for local in 0..DISPATCH_CORE_PARAM_COUNT {
            dispatch.instruction(&Ins::LocalGet(u32::try_from(local).unwrap()));
        }
        dispatch.instruction(&Ins::CallIndirect {
            ty: offset,
            table: 0,
        });
        dispatch.instruction(&Ins::End);

        code_section.function(&dispatch);

        let exports = summary
            .functions
            .iter()
            .filter_map(|f| {
                if let FunctionKind::Export = f.kind {
                    Some((f.interface.map(|i| i.name), f.name))
                } else {
                    None
                }
            })
            .collect::<IndexSet<_>>();
    }

    let mut elements = ElementSection::new();
    elements.active(
        Some(0),
        &ConstExpr::global_get(table_base),
        RefType {
            nullable: true,
            heap_type: HeapType::Func,
        },
        Elements::Functions(
            &summary
                .functions
                .iter()
                .enumerate()
                .filter_map(|(index, function)| {
                    function
                        .is_dispatchable()
                        .then_some((import_function_count + index).try_into().unwrap())
                })
                .collect::<Vec<_>>(),
        ),
    );

    let mut names_data = Vec::new();
    for (code, names) in [(0x01_u8, &function_names), (0x07_u8, &global_names)] {
        let mut subsection = Vec::new();
        names.len().encode(&mut subsection);
        for (index, name) in names {
            index.encode(&mut subsection);
            name.encode(&mut subsection);
        }
        names_data.push(code);
        subsection.encode(&mut names_data);
    }

    let mut result = Module::new();
    result.section(&types);
    result.section(&imports);
    result.section(&functions);
    result.section(&exports);
    result.section(&elements);
    result.section(&code);
    result.section(&CustomSection {
        name: "name",
        data: &names_data,
    });
    result.section(&CustomSection {
        name: &format!("component-type:{}", resolve.worlds[world].name),
        data: &metadata::encode(resolve, world, wit_component::StringEncoding::UTF8, None)?,
    });

    Ok(result.finish())
}
