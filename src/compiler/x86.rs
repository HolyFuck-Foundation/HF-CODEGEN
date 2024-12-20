use alloc::string::{String, ToString};
use alloc::vec::Vec;

use hashbrown::HashMap;
use iced_x86::code_asm::{CodeLabel, *};
use iced_x86::BlockEncoderOptions;

use super::{CompilerError, CompilerErrorKind, CompilerSettings};
use crate::ir::{IrNode, IrOp};
use crate::scope::ScopeManager;
use crate::target::CallingConvention;

use object::endian::Endianness;
use object::write::{
    Architecture, BinaryFormat, Object, Relocation, RelocationEncoding, RelocationFlags,
    RelocationKind, SectionId, SectionKind, StandardSection, Symbol, SymbolFlags, SymbolId,
    SymbolKind, SymbolScope, SymbolSection,
};

pub struct Compiler {
    bitness: u32,
    calling_convention: CallingConvention,
    settings: CompilerSettings,
    external_calls: HashMap<String, Vec<CodeLabel>>,
    scopes: ScopeManager,
}

impl Compiler {
    pub fn new(
        bitness: u32,
        compiler_settings: CompilerSettings,
        calling_convention: CallingConvention,
    ) -> Self {
        Self {
            bitness,
            calling_convention,
            settings: compiler_settings,
            external_calls: HashMap::new(),
            scopes: ScopeManager::new(),
        }
    }

    fn add_external_call(&mut self, name: String, label: CodeLabel) {
        if let Some(v) = self.external_calls.get_mut(&name) {
            v.push(label);
        } else {
            self.external_calls.insert(name, vec![label]);
        }
    }

    /// Translates an IR node to x86 assembly and pushes it to the code assembler.
    ///
    /// # Registers
    ///
    /// R8: address of the current cell
    ///     access it via `byte_ptr(r8)` aka `byte ptr[r8]`
    ///
    /// TODO: we might wanna return the hashmap here
    fn translate_ir_node(
        &mut self,
        ir_node: Vec<IrNode>,
    ) -> Result<CodeAssemblerResult, CompilerError> {
        let mut code_asm = CodeAssembler::new(self.bitness).unwrap();
        for node in ir_node {
            self.translate_ir_node_impl(&mut code_asm, node)?;
        }
        code_asm
            .assemble_options(
                self.settings.base_address,
                BlockEncoderOptions::RETURN_RELOC_INFOS
                    | BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS,
            )
            .map_err(|e| CompilerError {
                kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                span: None,
            })
    }

    fn translate_function_impl(
        &mut self,
        code_asm: &mut CodeAssembler,
        name: String,
        span: crate::ir::Span,
        children: Vec<IrNode>,
    ) -> Result<(), CompilerError> {
        let mut fn_label = code_asm.create_label();

        code_asm.zero_bytes().map_err(|e| CompilerError {
            kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
            span: Some(span),
        })?;

        code_asm
            .set_label(&mut fn_label)
            .map_err(|e| CompilerError {
                kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                span: Some(span),
            })?;
        self.scopes.push_fn((name.clone(), fn_label));
        self.scopes.push_scope(name.clone());
        for fn_ir_node in children {
            self.translate_ir_node_impl(code_asm, fn_ir_node)?;
        }
        self.scopes.pop_scope();
        code_asm.ret().map_err(|e| CompilerError {
            kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
            span: Some(span),
        })?;

        Ok(())
    }

    fn translate_ir_node_impl(
        &mut self,
        code_asm: &mut CodeAssembler,
        ir_node: IrNode,
    ) -> Result<(), CompilerError> {
        match ir_node.node {
            IrOp::Add(n) => {
                let mut rem = n;
                while rem > 255 {
                    rem -= 255;
                    code_asm
                        // add byte ptr[r8], n
                        .add(byte_ptr(r8), 255 as u32)
                        .map_err(|e| CompilerError {
                            kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                            span: Some(ir_node.span),
                        })?;
                }
                code_asm
                    // add byte ptr[r8], n
                    .add(byte_ptr(r8), rem as u32)
                    .map_err(|e| CompilerError {
                        kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                        span: Some(ir_node.span),
                    })?;
            }
            IrOp::Subtract(n) => {
                code_asm
                    // sub byte ptr[r8], n
                    .sub(byte_ptr(r8), n as u32)
                    .map_err(|e| CompilerError {
                        kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                        span: Some(ir_node.span),
                    })?;
            }
            IrOp::MoveRight(n) => {
                if n > 0x7FFFFFFF {
                    return Err(CompilerError {
                        kind: super::CompilerErrorKind::MoveTooLarge(n as u32),
                        span: Some(ir_node.span),
                    });
                }
                code_asm
                    // lea r8, [r8 + n]
                    .lea(r8, dword_ptr(r8 + n as u32))
                    .map_err(|e| CompilerError {
                        kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                        span: Some(ir_node.span),
                    })?;
            }
            IrOp::MoveLeft(n) => {
                if n > 0x7FFFFFFF {
                    return Err(CompilerError {
                        kind: super::CompilerErrorKind::MoveTooLarge(n as u32),
                        span: Some(ir_node.span),
                    });
                }
                code_asm
                    // lea r8, [r8 - n]
                    .lea(r8, dword_ptr(r8 - n as u32))
                    .map_err(|e| CompilerError {
                        kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                        span: Some(ir_node.span),
                    })?;
            }
            IrOp::StackPush => {
                code_asm
                    .lea(r9, dword_ptr(r9 + 1))
                    .map_err(|e| CompilerError {
                        kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                        span: Some(ir_node.span),
                    })?;
                code_asm.mov(al, byte_ptr(r8)).map_err(|e| CompilerError {
                    kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                    span: Some(ir_node.span),
                })?;
                code_asm.mov(byte_ptr(r9), al).map_err(|e| CompilerError {
                    kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                    span: Some(ir_node.span),
                })?;
            }
            IrOp::StackPop => {
                code_asm.mov(al, byte_ptr(r9)).map_err(|e| CompilerError {
                    kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                    span: Some(ir_node.span),
                })?;
                code_asm.mov(byte_ptr(r8), al).map_err(|e| CompilerError {
                    kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                    span: Some(ir_node.span),
                })?;
                code_asm
                    .lea(r9, dword_ptr(r9 - 1))
                    .map_err(|e| CompilerError {
                        kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                        span: Some(ir_node.span),
                    })?;
            }
            // equivalent:
            //
            // while *r8 != 0 {
            //    // code
            // }
            //
            // or in assembly:
            //
            // start_label:
            //    cmp byte ptr[r8], 0
            //    je end_label
            //    ... ; code
            //    jmp start_label
            // end_label:
            //
            IrOp::Condition(cond_ir_nodes) => {
                let mut start_label = code_asm.create_label();
                let mut end_label = code_asm.create_label();

                // phantom instruction so we have an address
                code_asm.zero_bytes().map_err(|e| CompilerError {
                    kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                    span: Some(ir_node.span),
                })?;

                code_asm
                    .set_label(&mut start_label)
                    .map_err(|e| CompilerError {
                        kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                        span: Some(ir_node.span),
                    })?;

                code_asm.cmp(byte_ptr(r8), 0).map_err(|e| CompilerError {
                    kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                    span: Some(ir_node.span),
                })?;
                code_asm.je(end_label).map_err(|e| CompilerError {
                    kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                    span: Some(ir_node.span),
                })?;

                let scope_name = format!(
                    "{};{}",
                    self.scopes.get_top_scope_name().unwrap_or(String::new()),
                    self.scopes.next_unnamed_scope_number()
                );
                self.scopes.push_scope(scope_name);
                for cond_ir_node in cond_ir_nodes {
                    self.translate_ir_node_impl(code_asm, cond_ir_node)?;
                }
                self.scopes.pop_scope();

                code_asm.jmp(start_label).map_err(|e| CompilerError {
                    kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                    span: Some(ir_node.span),
                })?;

                code_asm
                    .set_label(&mut end_label)
                    .map_err(|e| CompilerError {
                        kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                        span: Some(ir_node.span),
                    })?;

                // phantom instruction so we have an address
                code_asm.zero_bytes().map_err(|e| CompilerError {
                    kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                    span: Some(ir_node.span),
                })?;
            }
            IrOp::Function(name, fn_ir_nodes) => {
                self.translate_function_impl(code_asm, name, ir_node.span, fn_ir_nodes)?;
            }
            IrOp::FunctionCall(name) => {
                let fn_label = self.scopes.get_fn(&name).ok_or(CompilerError {
                    kind: CompilerErrorKind::FunctionNotFound(name),
                    span: Some(ir_node.span),
                })?;
                code_asm.call(fn_label).map_err(|e| CompilerError {
                    kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                    span: Some(ir_node.span),
                })?;
            }
            IrOp::ExternalFunctionCall(name) => {
                let mut label = code_asm.create_label();
                code_asm.zero_bytes().map_err(|e| CompilerError {
                    kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                    span: Some(ir_node.span),
                })?;
                // calling convention specific setup for the call
                match self.calling_convention {
                    CallingConvention::X86_64_SystemVAMD64 => {
                        // push r8 and r9 on the stack, then put the
                        // address of each stack element in rdi and rsi
                        code_asm.push(r8).map_err(|e| CompilerError {
                            kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                            span: Some(ir_node.span),
                        })?;
                        code_asm.push(r9).map_err(|e| CompilerError {
                            kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                            span: Some(ir_node.span),
                        })?;
                        code_asm
                            .lea(rdi, qword_ptr(rsp + 8))
                            .map_err(|e| CompilerError {
                                kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                                span: Some(ir_node.span),
                            })?;
                        code_asm
                            .lea(rsi, qword_ptr(rsp))
                            .map_err(|e| CompilerError {
                                kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                                span: Some(ir_node.span),
                            })?;
                    }
                    CallingConvention::X86_64_MicrosoftX64 => {
                        code_asm.push(r8).map_err(|e| CompilerError {
                            kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                            span: Some(ir_node.span),
                        })?;
                        code_asm.push(r9).map_err(|e| CompilerError {
                            kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                            span: Some(ir_node.span),
                        })?;
                        code_asm
                            .lea(rcx, qword_ptr(rsp + 8))
                            .map_err(|e| CompilerError {
                                kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                                span: Some(ir_node.span),
                            })?;
                        code_asm
                            .lea(rdx, qword_ptr(rsp))
                            .map_err(|e| CompilerError {
                                kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                                span: Some(ir_node.span),
                            })?;
                    }
                    _ => todo!(),
                }
                // call
                code_asm.set_label(&mut label).map_err(|e| CompilerError {
                    kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                    span: Some(ir_node.span),
                })?;
                self.add_external_call(name, label);
                code_asm.call(label).map_err(|e| CompilerError {
                    kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                    span: Some(ir_node.span),
                })?;
                // calling convention specific cleanup for the call
                match self.calling_convention {
                    CallingConvention::X86_64_SystemVAMD64 => {
                        code_asm.pop(r9).map_err(|e| CompilerError {
                            kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                            span: Some(ir_node.span),
                        })?;
                        code_asm.pop(r8).map_err(|e| CompilerError {
                            kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                            span: Some(ir_node.span),
                        })?;
                    }
                    CallingConvention::X86_64_MicrosoftX64 => {
                        code_asm.pop(r9).map_err(|e| CompilerError {
                            kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                            span: Some(ir_node.span),
                        })?;
                        code_asm.pop(r8).map_err(|e| CompilerError {
                            kind: super::CompilerErrorKind::AssemblerError(e.to_string()),
                            span: Some(ir_node.span),
                        })?;
                    }
                    _ => todo!(),
                }
            }
            _ => todo!(),
        }
        Ok(())
    }
}

impl super::CompilerTrait for Compiler {
    fn compile_to_bytecode(&mut self, ir: Vec<IrNode>) -> Result<Vec<u8>, CompilerError> {
        Ok(self.translate_ir_node(ir)?.inner.code_buffer)
    }

    fn compile_to_object_file(
        &mut self,
        ast: Vec<IrNode>,
        filename: &str,
    ) -> Result<Object, CompilerError> {
        let mut obj = Object::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
        obj.add_file_symbol(filename.as_bytes().to_vec());

        fn add_relocations_for_external_symbol(
            obj: &mut Object,
            section: SectionId,
            symbol: &str,
            call_sites: Vec<u64>,
        ) -> Result<(), CompilerError> {
            let alloc_sym = obj.add_symbol(Symbol {
                name: symbol.as_bytes().to_vec(),
                value: 0, // not our symbol, so we don't know the value
                size: 0,  // same here
                kind: SymbolKind::Text,
                scope: SymbolScope::Dynamic,
                weak: false,
                section: SymbolSection::Undefined,
                flags: SymbolFlags::None,
            });
            for call_site in call_sites {
                obj.add_relocation(
                    section,
                    Relocation {
                        // the +1 here is crucial because the address of the call doesn't start until
                        // one byte in (skips e8), we basically want to tell the linker "please replace the
                        // 32 bits at this address with the address of the symbol"
                        offset: call_site + 1,
                        symbol: alloc_sym,
                        addend: -4,
                        flags: RelocationFlags::Generic {
                            kind: RelocationKind::Relative,
                            encoding: RelocationEncoding::X86RipRelative,
                            size: 32, // size of the address to replace
                        },
                    },
                )
                .map_err(|e| CompilerError {
                    kind: CompilerErrorKind::RelocationFailed(e.to_string()),
                    span: None,
                })?;
            }
            Ok(())
        }

        let text_section = obj.add_section(Vec::new(), b".text".to_vec(), SectionKind::Text);

        let mut fn_symbol_map = HashMap::new();

        let mut fn_ast = Vec::new();
        let mut non_fn_ast = Vec::new();
        for node in ast {
            match &node.node {
                IrOp::Function(name, _children) => {
                    let name_bytes = name.as_bytes().to_vec();
                    let fn_symbol = obj.add_symbol(Symbol {
                        name: name_bytes.clone(),
                        value: 0,
                        size: 0,
                        kind: SymbolKind::Text,
                        scope: SymbolScope::Dynamic,
                        weak: false,
                        section: SymbolSection::Section(text_section),
                        flags: SymbolFlags::None,
                    });

                    fn_symbol_map.insert(name.clone(), fn_symbol);
                    fn_ast.push(node);
                }
                _ => non_fn_ast.push(node),
            }
        }

        fn_ast.push(IrNode {
            node: IrOp::Function("_start".to_string(), non_fn_ast),
            span: crate::ir::Span {
                location: (0, 0),
                length: 1,
            },
        });
        let mut result = self.translate_ir_node(fn_ast)?;

        for (name, label) in self.scopes.get_global_functions() {
            let name_bytes = name.as_bytes().to_vec();
            let _fn_symbol = obj.add_symbol(Symbol {
                name: name_bytes.clone(),
                value: result.label_ip(&label).expect("couldnt find label ip"),
                size: 0,
                kind: SymbolKind::Text,
                scope: SymbolScope::Dynamic,
                weak: false,
                section: SymbolSection::Section(text_section),
                flags: SymbolFlags::None,
            });
        }

        // Because of iced-x86 shenanigans, we must force the call bytes to zero
        // for any externals we try to call.
        // Sorry :(
        for (_name, labels) in &self.external_calls {
            for label in labels {
                let ip = result
                    .label_ip(label)
                    .expect("couldnt find label ip for external call");
                for i in (ip + 1)..(ip + 5) {
                    result.inner.code_buffer[i as usize] = 0;
                }
            }
        }

        let name_bytes = b"_start".to_vec();
        let fn_symbol = obj.add_symbol(Symbol {
            name: name_bytes.clone(),
            value: 0,
            size: 0,
            kind: SymbolKind::Text,
            scope: SymbolScope::Dynamic,
            weak: false,
            section: SymbolSection::Section(text_section),
            flags: SymbolFlags::None,
        });

        let _fn_offset =
            obj.add_symbol_data(fn_symbol, text_section, &result.inner.code_buffer, 16);

        // Update the IP for our start symbol
        let label = self
            .scopes
            .get_fn(&"_start".to_string())
            .expect("couldnt find function label for _start");
        let ip = result
            .label_ip(&label)
            .expect("couldnt find label ip for _start");
        obj.set_symbol_data(fn_symbol, text_section, ip, 0);

        // Map from a
        // HashMap<String, Vec<CodeLabel>>
        // to a
        // HashMap<String, Vec<u64>>
        // where each u64 is the ip of the label
        let externals = self.external_calls.iter().map(|(name, label_vec)| {
            (
                name,
                label_vec
                    .iter()
                    .map(|label| result.label_ip(label).unwrap() as u64)
                    .collect::<Vec<u64>>(),
            )
        });
        for (name, call_sites) in externals {
            add_relocations_for_external_symbol(&mut obj, text_section, &name, call_sites)?;
        }

        // Update the IP for symbols
        for (name, symbol_id) in fn_symbol_map {
            let label = self
                .scopes
                .get_fn(&name)
                .expect("couldnt find function label");
            let ip = result.label_ip(&label).expect("couldnt find label ip");
            obj.set_symbol_data(symbol_id, text_section, ip, 0);
        }

        Ok(obj)
    }
}
