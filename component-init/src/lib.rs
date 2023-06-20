use {
    anyhow::{bail, Result},
    std::{
        collections::{hash_map::Entry, HashMap},
        iter,
    },
    wasm_convert::{IntoConstExpr, IntoExportKind, IntoGlobalType, IntoMemoryType},
    wasm_encoder::{
        Alias, CanonicalFunctionSection, CanonicalOption, CodeSection, Component,
        ComponentAliasSection, ComponentExportKind, ComponentExportSection, ComponentExternName,
        ComponentTypeSection, ComponentValType, ConstExpr, DataSection, ExportKind, ExportSection,
        Function, FunctionSection, GlobalSection, GlobalType, ImportSection, InstanceSection,
        Instruction as Ins, MemArg, MemoryType, Module, ModuleArg, ModuleSection, PrimitiveValType,
        RawSection, TypeSection, ValType,
    },
    wasmparser::{
        ComponentAlias, Encoding, ExternalKind, Instance, Operator, Parser, Payload, TypeRef,
    },
};

const PAGE_SIZE_BYTES: i32 = 64 * 1024;
const MAX_CONSECUTIVE_ZEROS: usize = 8;

pub trait Invoker {
    fn call_s32(&mut self, function: &str) -> Result<i32>;
    fn call_s64(&mut self, function: &str) -> Result<i64>;
    fn call_float32(&mut self, function: &str) -> Result<f32>;
    fn call_float64(&mut self, function: &str) -> Result<f64>;
    fn call_list_u8(&mut self, function: &str) -> Result<Vec<u8>>;
}

fn get_and_increment(n: &mut u32) -> u32 {
    let v = *n;
    *n += 1;
    v
}

pub fn mem_arg(offset: u64, align: u32) -> MemArg {
    MemArg {
        offset,
        align,
        memory_index: 0,
    }
}

pub fn initialize(
    component: &[u8],
    initialize: impl FnOnce(&[u8]) -> Result<Box<dyn Invoker>>,
) -> Result<Vec<u8>> {
    // First, instrument the input component, validating that it conforms to certain rules and exposing the memory
    // and all mutable globals via synthesized function exports.
    //
    // Note that we currently only support a certain style of component, but plan to eventually generalize this
    // tool to support arbitrary component graphs.
    //
    // Current rules:
    // - Flat structure (i.e. no subcomponents)
    // - Single memory
    // - Single table
    // - No runtime table operations
    // - No reference type globals
    // - Each module instantiated at most once
    // - If a module exports a memory, a single module must export a mutable `__stack_pointer` global of type I32
    //
    // Note that we use `__stack_pointer` to allocate 8 bytes to store the canonical `list<u8>` representation of
    // memory.

    let copy_component_section = |section, component: &[u8], result: &mut Component| {
        if let Some((id, range)) = section {
            result.section(&RawSection {
                id,
                data: &component[range],
            });
        }
    };

    let copy_module_section = |section, module: &[u8], result: &mut Module| {
        if let Some((id, range)) = section {
            result.section(&RawSection {
                id,
                data: &module[range],
            });
        }
    };

    let mut module_count = 0;
    let mut instance_count = 0;
    let mut core_function_count = 0;
    let mut type_count = 0;
    let mut memory_info = None;
    let mut saw_table = false;
    let mut globals_to_export = HashMap::<_, HashMap<_, _>>::new();
    let mut instantiations = HashMap::new();
    let mut stack_pointer_exports = Vec::new();
    let mut instrumented_component = Component::new();
    for payload in Parser::new(0).parse_all(component) {
        let payload = payload?;
        let section = payload.as_section();
        match payload {
            Payload::Version { encoding, .. } => {
                if !matches!(encoding, Encoding::Component) {
                    bail!("expected component; got {encoding:?}");
                }
                copy_component_section(section, component, &mut instrumented_component);
            }

            Payload::ModuleSection { parser, range } => {
                let module = &component[range];
                let mut global_types = Vec::new();
                let mut empty = HashMap::new();
                let mut instrumented_module = Module::new();
                let module_index = get_and_increment(&mut module_count);
                let mut global_count = 0;
                for payload in parser.parse_all(module) {
                    let payload = payload?;
                    let section = payload.as_section();
                    match payload {
                        Payload::ImportSection(reader) => {
                            for import in reader {
                                if let TypeRef::Global(_) = import?.ty {
                                    global_count += 1;
                                }
                            }
                            copy_module_section(section, module, &mut instrumented_module);
                        }

                        Payload::TableSection(reader) => {
                            for _ in reader {
                                if saw_table {
                                    bail!("only one table allowed per component");
                                }
                                saw_table = true;
                            }
                            copy_module_section(section, module, &mut instrumented_module);
                        }

                        Payload::MemorySection(reader) => {
                            for memory in reader {
                                if memory_info.is_some() {
                                    bail!("only one memory allowed per component");
                                }
                                memory_info = Some((
                                    module_index,
                                    "memory",
                                    MemoryType::from(IntoMemoryType(memory?)),
                                ));
                            }
                            copy_module_section(section, module, &mut instrumented_module);
                        }

                        Payload::GlobalSection(reader) => {
                            for global in reader {
                                let global = global?;
                                let ty = GlobalType::from(IntoGlobalType(global.ty));
                                global_types.push(ty);
                                let global_index = get_and_increment(&mut global_count);
                                if global.ty.mutable {
                                    globals_to_export
                                        .entry(module_index)
                                        .or_default()
                                        .insert(global_index, (None, ty.val_type));
                                }
                            }
                            copy_module_section(section, module, &mut instrumented_module);
                        }

                        Payload::ExportSection(reader) => {
                            let mut exports = ExportSection::new();
                            for export in reader {
                                let export = export?;
                                if let ExternalKind::Global = export.kind {
                                    if let Some((name, _)) = globals_to_export
                                        .get_mut(&module_index)
                                        .and_then(|map| map.get_mut(&export.index))
                                    {
                                        *name = Some(export.name.to_owned());
                                    }
                                    if export.name == "__stack_pointer" {
                                        stack_pointer_exports.push((
                                            module_index,
                                            global_types[usize::try_from(export.index).unwrap()],
                                        ));
                                    }
                                }
                                exports.export(
                                    export.name,
                                    IntoExportKind(export.kind).into(),
                                    export.index,
                                );
                            }

                            for (index, (name, _)) in globals_to_export
                                .get_mut(&module_index)
                                .unwrap_or(&mut empty)
                            {
                                if name.is_none() {
                                    let new_name = format!("component-init:{index}");
                                    exports.export(&new_name, ExportKind::Global, *index);
                                    *name = Some(new_name);
                                }
                            }

                            instrumented_module.section(&exports);
                        }

                        Payload::CodeSectionEntry(body) => {
                            for operator in body.get_operators_reader()? {
                                match operator? {
                                    Operator::TableCopy { .. }
                                    | Operator::TableFill { .. }
                                    | Operator::TableGrow { .. }
                                    | Operator::TableInit { .. }
                                    | Operator::TableSet { .. } => {
                                        bail!("table operations not allowed");
                                    }

                                    _ => (),
                                }
                            }
                            copy_module_section(section, module, &mut instrumented_module);
                        }

                        _ => copy_module_section(section, module, &mut instrumented_module),
                    }
                }
                instrumented_component.section(&ModuleSection(&instrumented_module));
            }

            Payload::InstanceSection(reader) => {
                for instance in reader {
                    let instance_index = get_and_increment(&mut instance_count);

                    if let Instance::Instantiate { module_index, .. } = instance? {
                        match instantiations.entry(module_index) {
                            Entry::Vacant(entry) => {
                                entry.insert(instance_index);
                            }
                            Entry::Occupied(_) => bail!("modules may be instantiated at most once"),
                        }
                    }
                }
                copy_component_section(section, component, &mut instrumented_component);
            }

            Payload::ComponentAliasSection(reader) => {
                for alias in reader {
                    if let ComponentAlias::CoreInstanceExport { .. } = alias? {
                        core_function_count += 1;
                    }
                }
                copy_component_section(section, component, &mut instrumented_component);
            }

            Payload::ComponentTypeSection(reader) => {
                for _ in reader {
                    type_count += 1;
                }
                copy_component_section(section, component, &mut instrumented_component);
            }

            _ => copy_component_section(section, component, &mut instrumented_component),
        }
    }

    let mut types = TypeSection::new();
    let mut imports = ImportSection::new();
    let mut functions = FunctionSection::new();
    let mut exports = ExportSection::new();
    let mut code = CodeSection::new();
    let mut aliases = ComponentAliasSection::new();
    let mut lifts = CanonicalFunctionSection::new();
    let mut component_types = ComponentTypeSection::new();
    let mut component_exports = ComponentExportSection::new();
    for (module_index, globals_to_export) in &globals_to_export {
        for (global_index, (name, ty)) in globals_to_export {
            let offset = types.len();
            types.function([], [*ty]);
            imports.import(
                &module_index.to_string(),
                name.as_deref().unwrap(),
                GlobalType {
                    val_type: *ty,
                    mutable: true,
                },
            );
            functions.function(offset);
            let mut function = Function::new([]);
            function.instruction(&Ins::GlobalGet(offset));
            function.instruction(&Ins::End);
            code.function(&function);
            let export_name = format!("component-init-get-{module_index}-{global_index}");
            exports.export(&export_name, ExportKind::Func, offset);
            aliases.alias(Alias::InstanceExport {
                instance: instance_count,
                kind: ComponentExportKind::Func,
                name: &export_name,
            });
            component_types
                .function()
                .params(iter::empty::<(_, ComponentValType)>())
                .result(match ty {
                    ValType::I32 => PrimitiveValType::S32,
                    ValType::I64 => PrimitiveValType::S64,
                    ValType::F32 => PrimitiveValType::Float32,
                    ValType::F64 => PrimitiveValType::Float64,
                    ValType::V128 => bail!("V128 not yet supported"),
                    ValType::Ref(_) => bail!("reference types not supported"),
                });
            lifts.lift(
                core_function_count + offset,
                type_count + offset,
                [CanonicalOption::UTF8],
            );
            component_exports.export(
                ComponentExternName::Kebab(&export_name),
                ComponentExportKind::Func,
                offset,
                None,
            );
        }
    }

    if let Some((module_index, name, ty)) = memory_info {
        let stack_module_index = match stack_pointer_exports.as_slice() {
            [(
                index,
                GlobalType {
                    val_type: ValType::I32,
                    mutable: true,
                },
            )] => index,

            _ => bail!(
                "component with memory must contain exactly one module which \
                 exports a mutable `__stack_pointer` global of type I32"
            ),
        };
        let offset = types.len();
        types.function([], [ValType::I32]);
        imports.import(&module_index.to_string(), name, ty);
        imports.import(
            &stack_module_index.to_string(),
            "__stack_pointer",
            GlobalType {
                val_type: ValType::I32,
                mutable: true,
            },
        );
        functions.function(offset);

        let mut function = Function::new([(1, ValType::I32)]);
        function.instruction(&Ins::GlobalGet(offset));
        function.instruction(&Ins::I32Const(8));
        function.instruction(&Ins::I32Sub);
        function.instruction(&Ins::LocalTee(0));
        function.instruction(&Ins::I32Const(0));
        function.instruction(&Ins::I32Store(mem_arg(0, 2)));
        function.instruction(&Ins::LocalGet(0));
        function.instruction(&Ins::MemoryGrow(0));
        function.instruction(&Ins::I32Const(PAGE_SIZE_BYTES));
        function.instruction(&Ins::I32Mul);
        function.instruction(&Ins::I32Store(mem_arg(4, 2)));
        function.instruction(&Ins::LocalGet(0));
        function.instruction(&Ins::End);
        code.function(&function);

        let export_name = "component-init-get-memory".to_owned();
        exports.export(&export_name, ExportKind::Func, offset);
        aliases.alias(Alias::InstanceExport {
            instance: instance_count,
            kind: ComponentExportKind::Func,
            name: &export_name,
        });
        component_types.defined_type().list(PrimitiveValType::U8);
        component_types
            .function()
            .params(iter::empty::<(_, ComponentValType)>())
            .result(ComponentValType::Type(offset));
        lifts.lift(
            core_function_count + offset,
            type_count + offset + 1,
            [CanonicalOption::UTF8],
        );
        component_exports.export(
            ComponentExternName::Kebab(&export_name),
            ComponentExportKind::Func,
            offset,
            None,
        );
    }

    let mut instances = InstanceSection::new();
    instances.instantiate(
        module_count,
        instantiations
            .into_iter()
            .map(|(module_index, instance_index)| {
                (
                    module_index.to_string(),
                    ModuleArg::Instance(instance_index),
                )
            }),
    );

    let mut module = Module::new();
    module.section(&types);
    module.section(&imports);
    module.section(&functions);
    module.section(&exports);
    module.section(&code);

    instrumented_component.section(&ModuleSection(&module));
    instrumented_component.section(&instances);
    instrumented_component.section(&aliases);
    instrumented_component.section(&lifts);
    instrumented_component.section(&component_types);
    instrumented_component.section(&component_exports);

    // Next, invoke the provided `initialize` function, which will return a trait object through which we can
    // invoke the functions we added above to capture the state of the initialized instance.

    let mut invoker = initialize(&instrumented_component.finish())?;

    let mut global_values = globals_to_export
        .iter()
        .map(|(module_index, globals_to_export)| {
            Ok((
                *module_index,
                globals_to_export
                    .iter()
                    .map(|(global_index, (_, ty))| {
                        let name = &format!("component-init-get-{module_index}-{global_index}");
                        Ok((
                            *global_index,
                            match ty {
                                ValType::I32 => ConstExpr::i32_const(invoker.call_s32(name)?),
                                ValType::I64 => ConstExpr::i64_const(invoker.call_s64(name)?),
                                ValType::F32 => ConstExpr::f32_const(invoker.call_float32(name)?),
                                ValType::F64 => ConstExpr::f64_const(invoker.call_float64(name)?),
                                ValType::V128 => bail!("V128 not yet supported"),
                                ValType::Ref(_) => bail!("reference types not supported"),
                            },
                        ))
                    })
                    .collect::<Result<HashMap<_, _>>>()?,
            ))
        })
        .collect::<Result<HashMap<_, _>>>()?;

    let memory_value = memory_info.map(|_| invoker.call_list_u8("component-init-get-memory"));

    // Finally, create a new component, identical to the original except with all mutable globals initialized to
    // the snapshoted values, with all data sections and start functions removed, and with a single active data
    // section added containing the memory snapshot.

    let mut initialized_component = Component::new();
    let mut module_count = 0;
    for payload in Parser::new(0).parse_all(component) {
        let payload = payload?;
        let section = payload.as_section();
        match payload {
            Payload::ModuleSection { parser, range } => {
                let module = &component[range];
                let mut initialized_module = Module::new();
                let module_index = get_and_increment(&mut module_count);
                let mut global_values = global_values.remove(&module_index);
                let mut global_count = 0;
                for payload in parser.parse_all(module) {
                    let payload = payload?;
                    let section = payload.as_section();
                    match payload {
                        Payload::ImportSection(reader) => {
                            for import in reader {
                                if let TypeRef::Global(_) = import?.ty {
                                    global_count += 1;
                                }
                            }
                            copy_module_section(section, module, &mut initialized_module);
                        }

                        Payload::GlobalSection(reader) => {
                            let mut globals = GlobalSection::new();
                            for global in reader {
                                let global = global?;
                                let global_index = get_and_increment(&mut global_count);
                                globals.global(
                                    IntoGlobalType(global.ty).into(),
                                    &if global.ty.mutable {
                                        global_values
                                            .as_mut()
                                            .unwrap()
                                            .remove(&global_index)
                                            .unwrap()
                                    } else {
                                        IntoConstExpr(global.init_expr).into()
                                    },
                                );
                            }
                            initialized_module.section(&globals);
                        }

                        Payload::DataSection(_) | Payload::StartSection { .. } => (),

                        _ => copy_module_section(section, module, &mut initialized_module),
                    }
                }

                if matches!(memory_info, Some((index, ..)) if index == module_index) {
                    let value = memory_value.as_deref().unwrap();
                    let mut data = DataSection::new();
                    for (start, len) in Segments::new(value) {
                        data.active(
                            0,
                            &ConstExpr::i32_const(start.try_into().unwrap()),
                            value[start..][..len].iter().copied(),
                        );
                    }
                }
            }

            _ => copy_component_section(section, component, &mut initialized_component),
        }
    }

    Ok(initialized_component.finish())
}

struct Segments<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Segments<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }
}

impl<'a> Iterator for Segments<'a> {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<Self::Item> {
        let mut zero_count = 0;
        let mut start = 0;
        let mut length = 0;
        for (index, value) in self.bytes[self.offset..].iter().enumerate() {
            if *value == 0 {
                zero_count += 1;
            } else {
                if zero_count > MAX_CONSECUTIVE_ZEROS {
                    if length > 0 {
                        start += self.offset;
                        self.offset += index;
                        return Some((start, length));
                    } else {
                        start = index;
                        length = 1;
                    }
                } else {
                    length += zero_count + 1;
                }
                zero_count = 0;
            }
        }
        if length > 0 {
            start += self.offset;
            self.offset = self.bytes.len();
            Some((start, length))
        } else {
            self.offset = self.bytes.len();
            None
        }
    }
}
