//! The Quake virtual machine, ported from `vm.c` and `vm_interpreted.c`.
//!
//! A `.qvm` file is a header, a code segment and a data segment, all
//! little-endian. `VM_Create` reads that image, rounds the data segment up to
//! a power of two so every data access can be masked, and keeps a table of
//! instruction start offsets for jumps. [`Vm::load`] does the same, and
//! [`Vm::call`] runs bytecode in the loop from `VM_CallInterpreted`.
//!
//! Only the interpreter is ported: the native `.so` path in `VM_Create` and
//! the x86 compiler in `vm_x86.c` are absent, so a module always runs
//! interpreted. That is what the C engine does when it falls back after a
//! failed dll load, and it is enough to run the game, cgame and ui modules
//! unmodified.
//!
//! Four behaviours differ from the C loop, all of them crashes there:
//! dividing by zero, an out-of-range jump, a stack that overflows, and data
//! accesses outside the segment return [`VmError`] instead of faulting. Two
//! quirks are kept: `OP_BCOM` writes the complement to the slot below the top
//! without popping (as in `vm_interpreted.c`), and shift counts are masked to
//! five bits as x86 does.

use std::fmt;

/// `VM_MAGIC` from `qfiles.h`.
pub const VM_MAGIC: u32 = 0x1272_1444;
/// `MAX_STACK` in `vm_interpreted.c`: cells in the interpreter's own stack.
pub const MAX_STACK: usize = 256;
/// `STACK_SIZE`: bytes of the data segment reserved for the program stack.
const STACK_SIZE: u32 = 0x2_0000;
/// `sizeof( vmHeader_t )`.
const HEADER_LEN: usize = 32;
/// Refuse a data segment larger than this rather than allocating whatever a
/// corrupt header asks for. Stock modules are well under a megabyte.
const MAX_DATA: u32 = 64 << 20;

/// Opcodes, in the order of `opcode_t` in `vm_local.h`.
pub mod op {
    pub const UNDEF: u8 = 0;
    pub const IGNORE: u8 = 1;
    pub const BREAK: u8 = 2;
    pub const ENTER: u8 = 3;
    pub const LEAVE: u8 = 4;
    pub const CALL: u8 = 5;
    pub const PUSH: u8 = 6;
    pub const POP: u8 = 7;
    pub const CONST: u8 = 8;
    pub const LOCAL: u8 = 9;
    pub const JUMP: u8 = 10;
    pub const EQ: u8 = 11;
    pub const NE: u8 = 12;
    pub const LTI: u8 = 13;
    pub const LEI: u8 = 14;
    pub const GTI: u8 = 15;
    pub const GEI: u8 = 16;
    pub const LTU: u8 = 17;
    pub const LEU: u8 = 18;
    pub const GTU: u8 = 19;
    pub const GEU: u8 = 20;
    pub const EQF: u8 = 21;
    pub const NEF: u8 = 22;
    pub const LTF: u8 = 23;
    pub const LEF: u8 = 24;
    pub const GTF: u8 = 25;
    pub const GEF: u8 = 26;
    pub const LOAD1: u8 = 27;
    pub const LOAD2: u8 = 28;
    pub const LOAD4: u8 = 29;
    pub const STORE1: u8 = 30;
    pub const STORE2: u8 = 31;
    pub const STORE4: u8 = 32;
    pub const ARG: u8 = 33;
    pub const BLOCK_COPY: u8 = 34;
    pub const SEX8: u8 = 35;
    pub const SEX16: u8 = 36;
    pub const NEGI: u8 = 37;
    pub const ADD: u8 = 38;
    pub const SUB: u8 = 39;
    pub const DIVI: u8 = 40;
    pub const DIVU: u8 = 41;
    pub const MODI: u8 = 42;
    pub const MODU: u8 = 43;
    pub const MULI: u8 = 44;
    pub const MULU: u8 = 45;
    pub const BAND: u8 = 46;
    pub const BOR: u8 = 47;
    pub const BXOR: u8 = 48;
    pub const BCOM: u8 = 49;
    pub const LSH: u8 = 50;
    pub const RSHI: u8 = 51;
    pub const RSHU: u8 = 52;
    pub const NEGF: u8 = 53;
    pub const ADDF: u8 = 54;
    pub const SUBF: u8 = 55;
    pub const DIVF: u8 = 56;
    pub const MULF: u8 = 57;
    pub const CVIF: u8 = 58;
    pub const CVFI: u8 = 59;
}

/// The operands that follow an opcode, in bytes.
fn operand_len(opcode: u8) -> usize {
    match opcode {
        op::ENTER
        | op::CONST
        | op::LOCAL
        | op::LEAVE
        | op::EQ
        | op::NE
        | op::LTI
        | op::LEI
        | op::GTI
        | op::GEI
        | op::LTU
        | op::LEU
        | op::GTU
        | op::GEU
        | op::EQF
        | op::NEF
        | op::LTF
        | op::LEF
        | op::GTF
        | op::GEF
        | op::BLOCK_COPY => 4,
        op::ARG => 1,
        _ => 0,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmError {
    TruncatedHeader,
    BadMagic(u32),
    BadHeader,
    TruncatedCode,
    DataTooLarge(u32),
    DataOutOfRange,
    PcOutOfRange(usize),
    BadJump(i32),
    BadOpcode(u8),
    StackOverflow,
    StackUnderflow,
    StackUnbalanced(usize),
    DivideByZero,
    BadAlignment,
}

impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VmError::TruncatedHeader => write!(f, "the image is shorter than a vmHeader_t"),
            VmError::BadMagic(m) => write!(f, "bad vm magic 0x{m:08x}"),
            VmError::BadHeader => write!(f, "the header has an impossible length"),
            VmError::TruncatedCode => write!(f, "the code segment ends inside an instruction"),
            VmError::DataTooLarge(n) => write!(f, "a {n} byte data segment is too large"),
            VmError::DataOutOfRange => write!(f, "data access outside the segment"),
            VmError::PcOutOfRange(pc) => write!(f, "program counter {pc} outside the code"),
            VmError::BadJump(n) => write!(f, "jump to instruction {n}, which does not exist"),
            VmError::BadOpcode(opcode) => write!(f, "unknown opcode {opcode}"),
            VmError::StackOverflow => write!(f, "the interpreter stack overflowed"),
            VmError::StackUnderflow => write!(f, "the interpreter stack underflowed"),
            VmError::StackUnbalanced(n) => {
                write!(f, "the module returned with {n} cells on the stack, not 1")
            }
            VmError::DivideByZero => write!(f, "division by zero in the module"),
            VmError::BadAlignment => write!(f, "OP_BLOCK_COPY on unaligned addresses"),
        }
    }
}

impl std::error::Error for VmError {}

/// `vmHeader_t` from `qfiles.h`, as read from the image.
#[derive(Debug, Clone, Copy)]
struct Header {
    magic: u32,
    instruction_count: i32,
    code_offset: i32,
    code_length: i32,
    data_offset: i32,
    data_length: i32,
    lit_length: i32,
    bss_length: i32,
}

impl Header {
    fn read(bytes: &[u8]) -> Result<Self, VmError> {
        if bytes.len() < HEADER_LEN {
            return Err(VmError::TruncatedHeader);
        }
        let field = |i: usize| i32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
        Ok(Header {
            magic: field(0) as u32,
            instruction_count: field(1),
            code_offset: field(2),
            code_length: field(3),
            data_offset: field(4),
            data_length: field(5),
            lit_length: field(6),
            bss_length: field(7),
        })
    }
}

/// A loaded module: its code, its data segment and its program stack.
#[derive(Debug)]
pub struct Vm {
    name: String,
    code: Vec<u8>,
    instruction_pointers: Vec<u32>,
    data: Vec<u8>,
    data_mask: u32,
    program_stack: u32,
    stack_bottom: u32,
}

impl Vm {
    /// Loads a module from the bytes of a `.qvm` file, as `VM_Create` does
    /// after `FS_ReadFile`.
    pub fn load(name: impl Into<String>, bytes: &[u8]) -> Result<Self, VmError> {
        let header = Header::read(bytes)?;
        if header.magic != VM_MAGIC {
            return Err(VmError::BadMagic(header.magic));
        }
        if header.bss_length < 0
            || header.data_length < 0
            || header.lit_length < 0
            || header.code_length <= 0
            || header.instruction_count < 0
        {
            return Err(VmError::BadHeader);
        }

        let code_start = usize::try_from(header.code_offset).map_err(|_| VmError::BadHeader)?;
        let code_len = header.code_length as usize;
        let code = bytes
            .get(code_start..code_start + code_len)
            .ok_or(VmError::TruncatedCode)?
            .to_vec();

        // Round the data segment up to a power of two so every access can be
        // masked, exactly as `VM_Create` does.
        let data_start = usize::try_from(header.data_offset).map_err(|_| VmError::BadHeader)?;
        let initialized = header.data_length as u64 + header.lit_length as u64;
        let total = initialized + header.bss_length as u64;
        // Round in 64 bits so a header cannot overflow the sum before the
        // power-of-two step.
        let rounded = total.next_power_of_two();
        if rounded > u64::from(MAX_DATA) {
            return Err(VmError::DataTooLarge(rounded as u32));
        }
        let rounded = rounded as u32;
        let initialized = initialized as usize;
        let mut data = vec![0u8; rounded as usize];
        let segment = bytes
            .get(data_start..data_start + initialized)
            .ok_or(VmError::BadHeader)?;
        data[..initialized].copy_from_slice(segment);
        // The C loader byte-swaps every initialized dword here. The host is
        // little-endian and so is the image, so that copy is the identity and
        // there is nothing to do.

        let count = header.instruction_count as usize;
        let mut instruction_pointers = Vec::with_capacity(count);
        let mut pc = 0usize;
        for _ in 0..count {
            instruction_pointers.push(pc as u32);
            let opcode = *code.get(pc).ok_or(VmError::TruncatedCode)?;
            pc += 1 + operand_len(opcode);
            if pc > code.len() {
                return Err(VmError::TruncatedCode);
            }
        }

        let data_mask = rounded - 1;
        let program_stack = data_mask + 1;
        let stack_bottom = program_stack.saturating_sub(STACK_SIZE);

        Ok(Vm {
            name: name.into(),
            code,
            instruction_pointers,
            data,
            data_mask,
            program_stack,
            stack_bottom,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The data segment. Pointers the module passes to a syscall are offsets
    /// into this, which is what `VM_ArgPtr` turns back into an address.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    /// The mask applied to every data access.
    pub fn data_mask(&self) -> u32 {
        self.data_mask
    }

    /// Reads a NUL-terminated string from a cell the module passed, as
    /// `VM_ArgPtr` plus `Q_strlen` would. Returns `None` when the offset does
    /// not land inside the segment.
    pub fn string_at(&self, cell: i32) -> Option<&str> {
        if cell == 0 {
            return None;
        }
        let start = (cell as u32 & self.data_mask) as usize;
        let bytes = self.data.get(start..)?;
        let end = bytes.iter().position(|&b| b == 0)?;
        std::str::from_utf8(&bytes[..end]).ok()
    }

    /// Calls entry point `call_number` with up to ten arguments, as
    /// `VM_Call` does. `syscall` receives the data segment, the import number
    /// and its arguments, and its return value goes back to the module.
    ///
    /// The import number is the low half of `args[0]` in the C path: the
    /// module passes `-1 - number`, and `VM_CallInterpreted` negates that
    /// again before handing the cells to the syscall table.
    pub fn call<F>(
        &mut self,
        call_number: i32,
        arguments: &[i32],
        mut syscall: F,
    ) -> Result<i32, VmError>
    where
        F: FnMut(&mut [u8], i32, &[i32; 10]) -> i32,
    {
        let stack_on_entry = self.program_stack;
        // The frame the module starts in: ten argument cells, a return-stack
        // cell and the -1 that ends the loop, as `VM_CallInterpreted` lays it
        // out.
        if stack_on_entry < 48 {
            return Err(VmError::StackOverflow);
        }
        let mut program_stack = stack_on_entry as i64 - 48;

        let mut cells = [0i32; 10];
        cells[0] = call_number;
        for (i, cell) in cells.iter_mut().enumerate().skip(1) {
            *cell = arguments.get(i - 1).copied().unwrap_or(0);
        }
        for (i, value) in cells.iter().enumerate() {
            store_at(&mut self.data, program_stack + 8 + 4 * i as i64, *value)?;
        }
        store_at(&mut self.data, program_stack + 4, 0)?;
        store_at(&mut self.data, program_stack, -1)?;

        let Vm {
            code,
            instruction_pointers,
            data,
            data_mask,
            stack_bottom,
            ..
        } = self;
        let code: &[u8] = code;
        let mask = *data_mask;
        let stack_bottom = *stack_bottom as i64;

        let mut stack = [0i32; MAX_STACK];
        let mut sp = 0usize;
        let mut pc = 0usize;

        macro_rules! push {
            ($value:expr) => {{
                if sp >= MAX_STACK {
                    return Err(VmError::StackOverflow);
                }
                stack[sp] = $value;
                sp += 1;
            }};
        }
        macro_rules! top {
            () => {{
                if sp == 0 {
                    return Err(VmError::StackUnderflow);
                }
                stack[sp - 1]
            }};
        }
        macro_rules! pop2 {
            () => {{
                if sp < 2 {
                    return Err(VmError::StackUnderflow);
                }
                let pair = (stack[sp - 2], stack[sp - 1]);
                sp -= 2;
                pair
            }};
        }
        macro_rules! argument {
            () => {{
                let value = read_i32(code, pc)?;
                pc += 4;
                value
            }};
        }

        let result = 'run: loop {
            let opcode = *code.get(pc).ok_or(VmError::PcOutOfRange(pc))?;
            pc += 1;

            match opcode {
                // NOPs: the C switch has no case for these, so they fall
                // through to the next instruction.
                op::UNDEF | op::IGNORE | op::BREAK => {}

                op::ENTER => {
                    let frame = argument!();
                    program_stack -= frame as i64;
                    if program_stack < stack_bottom {
                        return Err(VmError::StackOverflow);
                    }
                }
                op::LEAVE => {
                    // `OP_LEAVE` always transfers control: either back to the
                    // caller or out of the VM, so its operand never falls
                    // through and `pc` is replaced either way.
                    let frame = read_i32(code, pc)?;
                    program_stack += frame as i64;
                    let saved = load_at(data, program_stack)?;
                    if saved == -1 {
                        if sp != 1 {
                            return Err(VmError::StackUnbalanced(sp));
                        }
                        break 'run stack[0];
                    }
                    pc = usize::try_from(saved).map_err(|_| VmError::BadJump(saved))?;
                }
                op::CALL => {
                    let target = top!();
                    sp -= 1;
                    // The return address lives at the frame base, under the
                    // locals the callee is about to reserve.
                    store_at(data, program_stack, pc as i32)?;
                    if target < 0 {
                        let number = -1 - target;
                        // The import number takes the return-stack slot, and
                        // the arguments follow it, so the syscall table reads
                        // one array starting there.
                        store_at(data, program_stack + 4, number)?;
                        let mut parms = [0i32; 10];
                        for (i, cell) in parms.iter_mut().enumerate() {
                            *cell = load_at(data, program_stack + 4 + 4 * i as i64)?;
                        }
                        let returned = syscall(data, number, &parms);
                        push!(returned);
                        pc = load_at(data, program_stack)? as usize;
                    } else {
                        pc = jump_target(instruction_pointers, target)?;
                    }
                }
                op::PUSH => push!(0),
                op::POP => {
                    if sp == 0 {
                        return Err(VmError::StackUnderflow);
                    }
                    sp -= 1;
                }

                op::CONST => push!(argument!()),
                op::LOCAL => {
                    let offset = argument!() as i64;
                    push!((program_stack + offset) as i32);
                }

                op::LOAD4 => {
                    let address = top!() as i64;
                    stack[sp - 1] = load_i32(data, mask, address)?;
                }
                op::LOAD2 => {
                    let address = top!() as i64;
                    stack[sp - 1] = load_u16(data, mask, address)? as i32;
                }
                op::LOAD1 => {
                    let address = top!() as i64;
                    stack[sp - 1] = load_u8(data, mask, address)? as i32;
                }
                op::STORE4 => {
                    let (address, value) = pop2!();
                    store_i32(data, mask, address as i64, value)?;
                }
                op::STORE2 => {
                    let (address, value) = pop2!();
                    store_u16(data, mask, address as i64, value as u16)?;
                }
                op::STORE1 => {
                    let (address, value) = pop2!();
                    store_u8(data, mask, address as i64, value as u8)?;
                }
                op::ARG => {
                    let offset = *code.get(pc).ok_or(VmError::PcOutOfRange(pc))? as i64;
                    pc += 1;
                    let value = top!();
                    sp -= 1;
                    store_at(data, program_stack + offset, value)?;
                }
                op::BLOCK_COPY => {
                    let count = argument!() as u32;
                    let (destination, source) = pop2!();
                    let srci = source as u32 & mask;
                    let desti = destination as u32 & mask;
                    // Clamp the length to the end of each masked range, as the
                    // C loop does.
                    let mut length = ((srci.wrapping_add(count)) & mask).wrapping_sub(srci);
                    length = ((desti.wrapping_add(length)) & mask).wrapping_sub(desti);
                    if (source | destination | length as i32) & 3 != 0 {
                        return Err(VmError::BadAlignment);
                    }
                    let mut i = length as usize;
                    while i >= 4 {
                        i -= 4;
                        let word = load_at(data, srci as i64 + i as i64)?;
                        store_at(data, desti as i64 + i as i64, word)?;
                    }
                }

                op::JUMP => {
                    let target = top!();
                    sp -= 1;
                    pc = jump_target(instruction_pointers, target)?;
                }

                // Branches pop both operands, then either go to the target or
                // fall past the four operand bytes.
                op::EQ | op::NE | op::LTI | op::LEI | op::GTI | op::GEI | op::LTU | op::LEU
                | op::GTU | op::GEU | op::EQF | op::NEF | op::LTF | op::LEF | op::GTF
                | op::GEF => {
                    let target = argument!();
                    let (left, right) = pop2!();
                    let taken = match opcode {
                        op::EQ => left == right,
                        op::NE => left != right,
                        op::LTI => left < right,
                        op::LEI => left <= right,
                        op::GTI => left > right,
                        op::GEI => left >= right,
                        op::LTU => (left as u32) < (right as u32),
                        op::LEU => (left as u32) <= (right as u32),
                        op::GTU => (left as u32) > (right as u32),
                        op::GEU => (left as u32) >= (right as u32),
                        op::EQF => f32::from_bits(left as u32) == f32::from_bits(right as u32),
                        op::NEF => f32::from_bits(left as u32) != f32::from_bits(right as u32),
                        op::LTF => f32::from_bits(left as u32) < f32::from_bits(right as u32),
                        op::LEF => f32::from_bits(left as u32) <= f32::from_bits(right as u32),
                        op::GTF => f32::from_bits(left as u32) > f32::from_bits(right as u32),
                        _ => f32::from_bits(left as u32) >= f32::from_bits(right as u32),
                    };
                    if taken {
                        pc = jump_target(instruction_pointers, target)?;
                    }
                }

                op::SEX8 => stack[sp - 1] = stack[sp - 1] as u8 as i8 as i32,
                op::SEX16 => stack[sp - 1] = stack[sp - 1] as u16 as i16 as i32,

                op::NEGI => stack[sp - 1] = stack[sp - 1].wrapping_neg(),
                op::ADD => {
                    let (left, right) = pop2!();
                    push!(left.wrapping_add(right));
                }
                op::SUB => {
                    let (left, right) = pop2!();
                    push!(left.wrapping_sub(right));
                }
                op::DIVI => {
                    let (left, right) = pop2!();
                    if right == 0 {
                        return Err(VmError::DivideByZero);
                    }
                    push!(left.wrapping_div(right));
                }
                op::DIVU => {
                    let (left, right) = pop2!();
                    if right == 0 {
                        return Err(VmError::DivideByZero);
                    }
                    push!(((left as u32) / (right as u32)) as i32);
                }
                op::MODI => {
                    let (left, right) = pop2!();
                    if right == 0 {
                        return Err(VmError::DivideByZero);
                    }
                    push!(left.wrapping_rem(right));
                }
                op::MODU => {
                    let (left, right) = pop2!();
                    if right == 0 {
                        return Err(VmError::DivideByZero);
                    }
                    push!(((left as u32) % (right as u32)) as i32);
                }
                op::MULI => {
                    let (left, right) = pop2!();
                    push!(left.wrapping_mul(right));
                }
                op::MULU => {
                    let (left, right) = pop2!();
                    push!((left as u32).wrapping_mul(right as u32) as i32);
                }
                op::BAND => {
                    let (left, right) = pop2!();
                    push!(left & right);
                }
                op::BOR => {
                    let (left, right) = pop2!();
                    push!(left | right);
                }
                op::BXOR => {
                    let (left, right) = pop2!();
                    push!(left ^ right);
                }
                // Quirk kept from the C loop: the result goes to the slot
                // below the top, and the stack pointer does not move.
                op::BCOM => {
                    if sp < 2 {
                        return Err(VmError::StackUnderflow);
                    }
                    stack[sp - 2] = !stack[sp - 1];
                }
                op::LSH => {
                    let (left, right) = pop2!();
                    push!(left.wrapping_shl((right as u32) & 31));
                }
                op::RSHI => {
                    let (left, right) = pop2!();
                    push!(left.wrapping_shr((right as u32) & 31));
                }
                op::RSHU => {
                    let (left, right) = pop2!();
                    push!(((left as u32) >> ((right as u32) & 31)) as i32);
                }

                op::NEGF => {
                    let value = f32::from_bits(stack[sp - 1] as u32);
                    stack[sp - 1] = (-value).to_bits() as i32;
                }
                op::ADDF | op::SUBF | op::DIVF | op::MULF => {
                    let (left, right) = pop2!();
                    let a = f32::from_bits(left as u32);
                    let b = f32::from_bits(right as u32);
                    let value = match opcode {
                        op::ADDF => a + b,
                        op::SUBF => a - b,
                        op::DIVF => a / b,
                        _ => a * b,
                    };
                    push!(value.to_bits() as i32);
                }
                op::CVIF => {
                    let value = stack[sp - 1] as f32;
                    stack[sp - 1] = value.to_bits() as i32;
                }
                op::CVFI => {
                    let value = f32::from_bits(stack[sp - 1] as u32);
                    // `as` saturates at the integer bounds, where the C cast
                    // is undefined outside them.
                    stack[sp - 1] = value as i32;
                }

                _ => return Err(VmError::BadOpcode(opcode)),
            }
        };

        self.program_stack = stack_on_entry;
        Ok(result)
    }
}

fn jump_target(instruction_pointers: &[u32], target: i32) -> Result<usize, VmError> {
    usize::try_from(target)
        .ok()
        .and_then(|target| instruction_pointers.get(target))
        .map(|&offset| offset as usize)
        .ok_or(VmError::BadJump(target))
}

fn read_i32(code: &[u8], at: usize) -> Result<i32, VmError> {
    let bytes = code.get(at..at + 4).ok_or(VmError::PcOutOfRange(at))?;
    Ok(i32::from_le_bytes(bytes.try_into().unwrap()))
}

/// Reads a cell at a masked address, as `*(int *)&image[address & dataMask]`.
fn load_i32(data: &[u8], mask: u32, address: i64) -> Result<i32, VmError> {
    load_at(data, (address as u32 & mask) as i64)
}

fn load_u16(data: &[u8], mask: u32, address: i64) -> Result<u16, VmError> {
    let at = (address as u32 & mask) as usize;
    let bytes = data.get(at..at + 2).ok_or(VmError::DataOutOfRange)?;
    Ok(u16::from_le_bytes(bytes.try_into().unwrap()))
}

fn load_u8(data: &[u8], mask: u32, address: i64) -> Result<u8, VmError> {
    data.get((address as u32 & mask) as usize)
        .copied()
        .ok_or(VmError::DataOutOfRange)
}

fn store_i32(data: &mut [u8], mask: u32, address: i64, value: i32) -> Result<(), VmError> {
    let at = (address as u32 & (mask & !3)) as i64;
    store_at(data, at, value)
}

fn store_u16(data: &mut [u8], mask: u32, address: i64, value: u16) -> Result<(), VmError> {
    let at = (address as u32 & (mask & !1)) as usize;
    let slot = data.get_mut(at..at + 2).ok_or(VmError::DataOutOfRange)?;
    slot.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn store_u8(data: &mut [u8], mask: u32, address: i64, value: u8) -> Result<(), VmError> {
    let at = (address as u32 & mask) as usize;
    *data.get_mut(at).ok_or(VmError::DataOutOfRange)? = value;
    Ok(())
}

/// An unmasked access, for the frame slots the interpreter addresses directly.
fn load_at(data: &[u8], at: i64) -> Result<i32, VmError> {
    let at = usize::try_from(at).map_err(|_| VmError::DataOutOfRange)?;
    let bytes = data.get(at..at + 4).ok_or(VmError::DataOutOfRange)?;
    Ok(i32::from_le_bytes(bytes.try_into().unwrap()))
}

fn store_at(data: &mut [u8], at: i64, value: i32) -> Result<(), VmError> {
    let at = usize::try_from(at).map_err(|_| VmError::DataOutOfRange)?;
    let slot = data.get_mut(at..at + 4).ok_or(VmError::DataOutOfRange)?;
    slot.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a code segment and the instruction table beside it, so a test
    /// can write bytecode without counting bytes.
    struct Asm {
        code: Vec<u8>,
        pointers: Vec<u32>,
    }

    impl Asm {
        fn new() -> Self {
            Asm {
                code: Vec::new(),
                pointers: Vec::new(),
            }
        }

        fn op(&mut self, opcode: u8) -> &mut Self {
            self.pointers.push(self.code.len() as u32);
            self.code.push(opcode);
            self
        }

        /// An instruction with a four-byte operand: `OP_CONST`, `OP_ENTER`,
        /// `OP_LEAVE` or a branch.
        fn op32(&mut self, opcode: u8, value: i32) -> &mut Self {
            self.op(opcode);
            self.code.extend_from_slice(&value.to_le_bytes());
            self
        }

        /// `OP_ARG`, whose operand is a single byte.
        fn arg(&mut self, offset: u8) -> &mut Self {
            self.op(op::ARG);
            self.code.push(offset);
            self
        }

        /// A complete image: header, code, then a data segment of `data_len`
        /// zero bytes.
        fn image(&self, data_len: usize) -> Vec<u8> {
            let data_offset = HEADER_LEN + self.code.len();
            let header = [
                VM_MAGIC as i32,
                self.pointers.len() as i32,
                HEADER_LEN as i32,
                self.code.len() as i32,
                data_offset as i32,
                data_len as i32,
                0,
                0,
            ];
            let mut image = Vec::new();
            for field in header {
                image.extend_from_slice(&field.to_le_bytes());
            }
            image.extend_from_slice(&self.code);
            image.resize(data_offset + data_len, 0);
            image
        }
    }

    /// No syscalls: a module that calls one gets zero back.
    fn no_syscalls(_: &mut [u8], _: i32, _: &[i32; 10]) -> i32 {
        0
    }

    #[test]
    fn constant_comes_back_as_the_return_value() {
        let mut asm = Asm::new();
        asm.op32(op::ENTER, 0).op32(op::CONST, 42).op32(op::LEAVE, 0);
        let image = asm.image(256);
        let mut vm = Vm::load("test", &image).unwrap();
        assert_eq!(vm.call(0, &[], no_syscalls), Ok(42));
    }

    #[test]
    fn arithmetic_uses_the_lower_operand_as_the_left_side() {
        let mut asm = Asm::new();
        asm.op32(op::ENTER, 0)
            .op32(op::CONST, 10)
            .op32(op::CONST, 3)
            .op(op::SUB)
            .op32(op::LEAVE, 0);
        let image = asm.image(256);
        let mut vm = Vm::load("test", &image).unwrap();
        assert_eq!(vm.call(0, &[], no_syscalls), Ok(7));
    }

    #[test]
    fn a_taken_branch_skips_the_false_arm() {
        // if (5 == 5) return 1; else return 99;
        // `OP_JUMP` takes its destination from the stack, not from an
        // operand, which is what q3asm emits: push the target, then jump.
        let mut asm = Asm::new();
        asm.op32(op::ENTER, 0)
            .op32(op::CONST, 5)
            .op32(op::CONST, 5)
            .op32(op::EQ, 7) // taken: jump to the "return 1" arm
            .op32(op::CONST, 99)
            .op32(op::CONST, 8) // skip the true arm
            .op(op::JUMP)
            .op32(op::CONST, 1)
            .op32(op::LEAVE, 0);
        let image = asm.image(256);
        let mut vm = Vm::load("test", &image).unwrap();
        assert_eq!(vm.call(0, &[], no_syscalls), Ok(1));
    }

    #[test]
    fn an_untaken_branch_falls_through_to_the_next_instruction() {
        let mut asm = Asm::new();
        asm.op32(op::ENTER, 0)
            .op32(op::CONST, 5)
            .op32(op::CONST, 6)
            .op32(op::EQ, 7)
            .op32(op::CONST, 99)
            .op32(op::CONST, 8)
            .op(op::JUMP)
            .op32(op::CONST, 1)
            .op32(op::LEAVE, 0);
        let image = asm.image(256);
        let mut vm = Vm::load("test", &image).unwrap();
        assert_eq!(vm.call(0, &[], no_syscalls), Ok(99));
    }

    #[test]
    fn locals_are_stored_and_loaded_through_the_program_stack() {
        let mut asm = Asm::new();
        asm.op32(op::ENTER, 8)
            .op32(op::LOCAL, 0)
            .op32(op::CONST, 42)
            .op(op::STORE4)
            .op32(op::LOCAL, 0)
            .op(op::LOAD4)
            .op32(op::LEAVE, 8);
        let image = asm.image(256);
        let mut vm = Vm::load("test", &image).unwrap();
        assert_eq!(vm.call(0, &[], no_syscalls), Ok(42));
    }

    #[test]
    fn a_syscall_gets_its_arguments_and_returns_its_result() {
        let mut asm = Asm::new();
        asm.op32(op::ENTER, 16)
            .op32(op::CONST, 7)
            .arg(8) // argument 0, at frame base + 8
            .op32(op::CONST, -3) // -1 - 2: import 2
            .op(op::CALL)
            .op32(op::LEAVE, 16);
        let image = asm.image(256);
        let mut vm = Vm::load("test", &image).unwrap();

        let seen = std::cell::Cell::new((0, 0, 0));
        let result = vm.call(0, &[], |_, number, parms| {
            seen.set((number, parms[0], parms[1]));
            123
        });
        assert_eq!(result, Ok(123));
        // The import number also lands in parms[0], as the C loop leaves it.
        assert_eq!(seen.get(), (2, 2, 7));
    }

    #[test]
    fn the_call_number_arrives_in_the_entry_frames_first_argument_cell() {
        // The entry frame keeps the call number at +8. `OP_ENTER 8` puts the
        // frame base one cell lower, so local 16 is that same cell.
        let mut asm = Asm::new();
        asm.op32(op::ENTER, 8)
            .op32(op::LOCAL, 16)
            .op(op::LOAD4)
            .op32(op::LEAVE, 8);
        let image = asm.image(256);
        let mut vm = Vm::load("test", &image).unwrap();
        assert_eq!(vm.call(9, &[], no_syscalls), Ok(9));
    }

    #[test]
    fn floats_convert_back_to_integers() {
        let mut asm = Asm::new();
        asm.op32(op::ENTER, 0)
            .op32(op::CONST, 1.5f32.to_bits() as i32)
            .op32(op::CONST, 2.25f32.to_bits() as i32)
            .op(op::ADDF)
            .op(op::CVFI)
            .op32(op::LEAVE, 0);
        let image = asm.image(256);
        let mut vm = Vm::load("test", &image).unwrap();
        assert_eq!(vm.call(0, &[], no_syscalls), Ok(3));
    }

    #[test]
    fn sign_extension_turns_a_byte_into_a_negative_cell() {
        let mut asm = Asm::new();
        asm.op32(op::ENTER, 0)
            .op32(op::CONST, 0xff)
            .op(op::SEX8)
            .op32(op::LEAVE, 0);
        let image = asm.image(256);
        let mut vm = Vm::load("test", &image).unwrap();
        assert_eq!(vm.call(0, &[], no_syscalls), Ok(-1));
    }

    #[test]
    fn block_copy_moves_words_from_the_source_below_the_top() {
        // Copy 8 bytes from address 0 to address 8, then load the copy.
        let mut asm = Asm::new();
        asm.op32(op::ENTER, 16)
            .op32(op::CONST, 0)
            .op32(op::CONST, 0x1122_3344)
            .op(op::STORE4)
            .op32(op::CONST, 8) // destination, below the source
            .op32(op::CONST, 0) // source, on top
            .op32(op::BLOCK_COPY, 8)
            .op32(op::CONST, 8)
            .op(op::LOAD4)
            .op32(op::LEAVE, 16);
        let image = asm.image(256);
        let mut vm = Vm::load("test", &image).unwrap();
        assert_eq!(vm.call(0, &[], no_syscalls), Ok(0x1122_3344));
    }

    #[test]
    fn data_is_padded_up_to_a_power_of_two() {
        let mut asm = Asm::new();
        asm.op32(op::ENTER, 0).op32(op::CONST, 1).op32(op::LEAVE, 0);
        let image = asm.image(100);
        let vm = Vm::load("test", &image).unwrap();
        assert_eq!(vm.data().len(), 128);
        assert_eq!(vm.data_mask(), 127);
    }

    #[test]
    fn a_bad_magic_is_refused() {
        let mut asm = Asm::new();
        asm.op32(op::ENTER, 0).op32(op::CONST, 1).op32(op::LEAVE, 0);
        let mut image = asm.image(256);
        image[0] ^= 0xff;
        assert!(matches!(Vm::load("test", &image), Err(VmError::BadMagic(_))));
    }

    #[test]
    fn a_truncated_image_is_refused() {
        assert!(matches!(
            Vm::load("test", &[0u8; 4]),
            Err(VmError::TruncatedHeader)
        ));
    }

    #[test]
    fn division_by_zero_is_an_error_not_a_panic() {
        let mut asm = Asm::new();
        asm.op32(op::ENTER, 0)
            .op32(op::CONST, 1)
            .op32(op::CONST, 0)
            .op(op::DIVI)
            .op32(op::LEAVE, 0);
        let image = asm.image(256);
        let mut vm = Vm::load("test", &image).unwrap();
        assert_eq!(vm.call(0, &[], no_syscalls), Err(VmError::DivideByZero));
    }

    #[test]
    fn a_jump_past_the_code_is_an_error() {
        let mut asm = Asm::new();
        asm.op32(op::ENTER, 0)
            .op32(op::CONST, 99)
            .op(op::JUMP)
            .op32(op::LEAVE, 0);
        let image = asm.image(256);
        let mut vm = Vm::load("test", &image).unwrap();
        assert_eq!(vm.call(0, &[], no_syscalls), Err(VmError::BadJump(99)));
    }

    #[test]
    fn strings_are_read_from_the_data_segment() {
        let mut asm = Asm::new();
        asm.op32(op::ENTER, 0).op32(op::CONST, 1).op32(op::LEAVE, 0);
        let mut image = asm.image(256);
        let data_offset = HEADER_LEN + asm.code.len();
        image[data_offset + 16..data_offset + 21].copy_from_slice(b"hi\0!!");
        let vm = Vm::load("test", &image).unwrap();
        assert_eq!(vm.string_at(16), Some("hi"));
        assert_eq!(vm.string_at(0), None);
    }
}
