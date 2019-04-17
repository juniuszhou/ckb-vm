use super::{
    super::{
        decoder::build_imac_decoder,
        instructions::{
            execute, instruction_length, is_basic_block_end_instruction, Instruction, Register,
        },
        memory::Memory,
        Error,
    },
    CoreMachine, DefaultMachine, Machine, SupportMachine,
};
use std::cmp::min;
use std::rc::Rc;

// The number of trace items to keep
const TRACE_SIZE: usize = 8192;
// Quick bit-mask to truncate a value in trace size range
const TRACE_MASK: usize = (TRACE_SIZE - 1);
// The maximum number of instructions to cache in a trace item
const TRACE_ITEM_LENGTH: usize = 16;
const TRACE_ITEM_MAXIMAL_ADDRESS_LENGTH: usize = 4 * TRACE_ITEM_LENGTH;
// Shifts to truncate a value so 2 traces has the minimal chance of sharing code.
const TRACE_ADDRESS_SHIFTS: usize = 5;

#[derive(Default)]
struct Trace {
    address: usize,
    length: usize,
    instruction_count: u8,
    instructions: [Instruction; TRACE_ITEM_LENGTH],
}

#[inline(always)]
fn calculate_slot(addr: usize) -> usize {
    (addr >> TRACE_ADDRESS_SHIFTS) & TRACE_MASK
}

pub struct TraceMachine<'a, Inner> {
    pub machine: DefaultMachine<'a, Inner>,

    traces: Vec<Trace>,
    running_trace_slot: usize,
    running_trace_cleared: bool,
}

impl<Inner: SupportMachine> CoreMachine for TraceMachine<'_, Inner> {
    type REG = <Inner as CoreMachine>::REG;
    type MEM = Self;

    fn pc(&self) -> &Self::REG {
        &self.machine.pc()
    }

    fn set_pc(&mut self, next_pc: Self::REG) {
        self.machine.set_pc(next_pc)
    }

    fn memory(&self) -> &Self {
        &self
    }

    fn memory_mut(&mut self) -> &mut Self {
        self
    }

    fn registers(&self) -> &[Self::REG] {
        self.machine.registers()
    }

    fn set_register(&mut self, idx: usize, value: Self::REG) {
        self.machine.set_register(idx, value)
    }
}

impl<Inner: SupportMachine> Memory<<Inner as CoreMachine>::REG> for TraceMachine<'_, Inner> {
    fn mmap(
        &mut self,
        addr: usize,
        size: usize,
        prot: u32,
        source: Option<Rc<Box<[u8]>>>,
        offset: usize,
    ) -> Result<(), Error> {
        self.machine
            .memory_mut()
            .mmap(addr, size, prot, source, offset)?;
        self.clear_traces(addr, size);
        Ok(())
    }

    fn munmap(&mut self, addr: usize, size: usize) -> Result<(), Error> {
        self.machine.memory_mut().munmap(addr, size)?;
        self.clear_traces(addr, size);
        Ok(())
    }

    fn store_byte(&mut self, addr: usize, size: usize, value: u8) -> Result<(), Error> {
        self.machine.memory_mut().store_byte(addr, size, value)?;
        self.clear_traces(addr, size);
        Ok(())
    }

    fn store_bytes(&mut self, addr: usize, value: &[u8]) -> Result<(), Error> {
        self.machine.memory_mut().store_bytes(addr, value)?;
        self.clear_traces(addr, value.len());
        Ok(())
    }

    fn execute_load16(&mut self, addr: usize) -> Result<u16, Error> {
        self.machine.memory_mut().execute_load16(addr)
    }

    fn load8(
        &mut self,
        addr: &<Inner as CoreMachine>::REG,
    ) -> Result<<Inner as CoreMachine>::REG, Error> {
        self.machine.memory_mut().load8(addr)
    }

    fn load16(
        &mut self,
        addr: &<Inner as CoreMachine>::REG,
    ) -> Result<<Inner as CoreMachine>::REG, Error> {
        self.machine.memory_mut().load16(addr)
    }

    fn load32(
        &mut self,
        addr: &<Inner as CoreMachine>::REG,
    ) -> Result<<Inner as CoreMachine>::REG, Error> {
        self.machine.memory_mut().load32(addr)
    }

    fn load64(
        &mut self,
        addr: &<Inner as CoreMachine>::REG,
    ) -> Result<<Inner as CoreMachine>::REG, Error> {
        self.machine.memory_mut().load64(addr)
    }

    fn store8(
        &mut self,
        addr: &<Inner as CoreMachine>::REG,
        value: &<Inner as CoreMachine>::REG,
    ) -> Result<(), Error> {
        self.machine.memory_mut().store8(addr, value)?;
        self.clear_traces(addr.to_usize(), 1);
        Ok(())
    }

    fn store16(
        &mut self,
        addr: &<Inner as CoreMachine>::REG,
        value: &<Inner as CoreMachine>::REG,
    ) -> Result<(), Error> {
        self.machine.memory_mut().store16(addr, value)?;
        self.clear_traces(addr.to_usize(), 2);
        Ok(())
    }

    fn store32(
        &mut self,
        addr: &<Inner as CoreMachine>::REG,
        value: &<Inner as CoreMachine>::REG,
    ) -> Result<(), Error> {
        self.machine.memory_mut().store32(addr, value)?;
        self.clear_traces(addr.to_usize(), 4);
        Ok(())
    }

    fn store64(
        &mut self,
        addr: &<Inner as CoreMachine>::REG,
        value: &<Inner as CoreMachine>::REG,
    ) -> Result<(), Error> {
        self.machine.memory_mut().store64(addr, value)?;
        self.clear_traces(addr.to_usize(), 8);
        Ok(())
    }
}

// NOTE: this might look redundant, but what we need is the execute method
// below works on TraceMachine instead of DefaultMachine, so we can leverage
// CoreMachine and Memory trait implementations above to clear related cached
// traces in case of a memory write.
impl<Inner: SupportMachine> Machine for TraceMachine<'_, Inner> {
    fn ecall(&mut self) -> Result<(), Error> {
        self.machine.ecall()
    }

    fn ebreak(&mut self) -> Result<(), Error> {
        self.machine.ebreak()
    }
}

impl<'a, Inner: SupportMachine> TraceMachine<'a, Inner> {
    pub fn new(machine: DefaultMachine<'a, Inner>) -> Self {
        Self {
            machine,
            traces: vec![],
            running_trace_slot: 0,
            running_trace_cleared: false,
        }
    }

    pub fn load_program(&mut self, program: &[u8], args: &[Vec<u8>]) -> Result<(), Error> {
        self.machine.load_program(program, args)?;
        Ok(())
    }

    pub fn run(&mut self) -> Result<u8, Error> {
        let decoder = build_imac_decoder::<Inner::REG>();
        self.machine.set_running(true);
        // For current trace size this is acceptable, however we might want
        // to tweak the code here if we choose to use a larger trace size or
        // larger trace item length.
        self.traces.resize_with(TRACE_SIZE, Trace::default);
        while self.machine.running() {
            let pc = self.pc().to_usize();
            let slot = calculate_slot(pc);
            if pc != self.traces[slot].address || self.traces[slot].instruction_count == 0 {
                self.traces[slot] = Trace::default();
                let mut current_pc = pc;
                let mut i = 0;
                while i < TRACE_ITEM_LENGTH {
                    let instruction = decoder.decode(self.memory_mut(), current_pc)?;
                    let end_instruction = is_basic_block_end_instruction(instruction);
                    current_pc += instruction_length(instruction);
                    self.traces[slot].instructions[i] = instruction;
                    i += 1;
                    if end_instruction {
                        break;
                    }
                }
                self.traces[slot].address = pc;
                self.traces[slot].length = current_pc - pc;
                self.traces[slot].instruction_count = i as u8;
            }
            self.running_trace_slot = slot;
            self.running_trace_cleared = false;
            for i in 0..self.traces[slot].instruction_count {
                let i = self.traces[slot].instructions[i as usize];
                execute(i, self)?;
                let cycles = self
                    .machine
                    .instruction_cycle_func()
                    .as_ref()
                    .map(|f| f(&i))
                    .unwrap_or(0);
                self.machine.add_cycles(cycles)?;
                if self.running_trace_cleared {
                    break;
                }
            }
        }
        Ok(self.machine.exit_code())
    }

    fn clear_traces(&mut self, address: usize, length: usize) {
        let end = address + length;
        let minimal_slot =
            calculate_slot(address.saturating_sub(TRACE_ITEM_MAXIMAL_ADDRESS_LENGTH));
        let maximal_slot = calculate_slot(end);
        for slot in minimal_slot..=min(maximal_slot, self.traces.len()) {
            let slot_address = self.traces[slot].address;
            let slot_end = slot_address + self.traces[slot].length;
            if !((end <= slot_address) || (slot_end <= address)) {
                self.traces[slot] = Trace::default();
                if self.running_trace_slot == slot {
                    self.running_trace_cleared = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::bits::power_of_2;
    use super::*;

    #[test]
    fn test_trace_constant_rules() {
        assert!(power_of_2(TRACE_SIZE));
        assert_eq!(TRACE_MASK, TRACE_SIZE - 1);
        assert!(power_of_2(TRACE_ITEM_LENGTH));
        assert!(TRACE_ITEM_LENGTH <= 255);
    }
}
