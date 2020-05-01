
pub(crate) mod chunk;
pub use chunk::Program;

use std::ops::{Deref, DerefMut};
use std::cmp::Reverse;
use std::collections::HashMap;
use std::convert::TryFrom;

use crate::{HissyError, ErrorType};
use crate::parser::{parse, ast::{Expr, Stat, Positioned, Block, Cond, BinOp, UnaOp}};
use crate::vm::{MAX_REGISTERS, InstrType};
use chunk::{Chunk, ChunkConstant};



fn error(s: String) -> HissyError {
	HissyError(ErrorType::Compilation, s, 0)
}
fn error_str(s: &str) -> HissyError {
	error(String::from(s))
}


fn emit_jump_to(chunk: &mut Chunk, add: usize) -> Result<(), HissyError> {
	let from = chunk.code.len();
	let to = add;
	let rel_jmp = to as isize - from as isize;
	let rel_jmp = i8::try_from(rel_jmp).map_err(|_| error_str("Jump too large"))?;
	chunk.emit_byte(rel_jmp as u8);
	Ok(())
}

fn fill_in_jump_from(chunk: &mut Chunk, add: usize) -> Result<(), HissyError> {
	let from = add;
	let to = chunk.code.len();
	let rel_jmp = to as isize - from as isize;
	let rel_jmp = i8::try_from(rel_jmp).map_err(|_| error_str("Jump too large"))?;
	chunk.code[add] = rel_jmp as u8;
	Ok(())
}

struct ChunkRegisters {
	required: u16,
	used: u16,
	local_cnt: u16,
}

impl ChunkRegisters {
	pub fn new() -> ChunkRegisters {
		ChunkRegisters {
			required: 0,
			used: 0,
			local_cnt: 0,
		}
	}
	
	pub fn new_reg(&mut self) -> Result<u8, HissyError> {
		let new_reg = u8::try_from(self.used).ok().filter(|r| *r < MAX_REGISTERS)
			.ok_or_else(|| error_str("Cannot compile: Too many registers required"))?;
		self.used += 1;
		if self.used > self.required {
			self.required = self.used
		}
		Ok(new_reg)
	}
	
	pub fn new_reg_range(&mut self, n: u16) -> Result<u8, HissyError> {
		u8::try_from(self.used + n - 1).ok().filter(|r| *r < MAX_REGISTERS)
			.ok_or_else(|| error_str("Cannot compile: Too many registers required"))?;
		let range_start = u8::try_from(self.used).unwrap();
		self.used += n;
		if self.used > self.required {
			self.required = self.used
		}
		Ok(range_start)
	}
	
	pub fn make_local(&mut self, i: u8) {
		assert!(u16::from(i) == self.local_cnt, "Local allocated above temporaries");
		self.local_cnt += 1;
	}
	
	// Marks register as freed
	pub fn free_reg(&mut self, i: u8) {
		assert!(u16::from(i) == self.used - 1, "Registers are not freed in FIFO order: {}, {}", i, self.used);
		self.used -= 1;
		if self.local_cnt > self.used {
			self.local_cnt = self.used;
		}
	}
	
	pub fn free_reg_range(&mut self, start: u8, n: u16) {
		assert!(u16::from(start) + n == self.used, "Registers are not freed in FIFO order");
		self.used -= n;
		if self.local_cnt > self.used {
			self.local_cnt = self.used;
		}
	}
	
	// Marks register as freed if temporary
	pub fn free_temp_reg(&mut self, i: u8) {
		if i < MAX_REGISTERS && u16::from(i) >= self.local_cnt {
			self.free_reg(i);
		}
	}
	
	pub fn free_temp_range(&mut self, start: u8, n: u16) {
		if u16::from(start) >= self.local_cnt {
			self.free_reg_range(start, n);
		}
	}
}


enum Binding {
	Local(u8),
	Upvalue(u8),
}

// Note: registers 128-255 correspond to constants in bytecode,
// but correspond to upvalues in the parent chunk in upvalue tables.

impl Binding {
	fn encoded(&self) -> u8 {
		match self {
			Binding::Local(reg) => *reg,
			Binding::Upvalue(upv) => upv + MAX_REGISTERS,
		}
	}
}


type BlockContext = HashMap<String, u8>;

struct UpvalueBinding {
	name: String,
	reg: u8,
}

struct ChunkContext {
	regs: ChunkRegisters,
	blocks: Vec<BlockContext>,
	upvalues: Vec<UpvalueBinding>,
}

impl ChunkContext {
	pub fn new() -> ChunkContext {
		ChunkContext {
			regs: ChunkRegisters::new(),
			blocks: Vec::new(),
			upvalues: Vec::new(),
		}
	}
	
	fn enter_block(&mut self) {
		self.blocks.push(BlockContext::new());
	}
	
	fn leave_block(&mut self) {
		let mut to_free: Vec<u8> = self.blocks.last().unwrap().values().copied().collect();
		to_free.sort_by_key(|&x| Reverse(x));
		for reg in to_free {
			self.regs.free_reg(reg);
		}
		self.blocks.pop();
	}
	
	fn find_block_local(&self, id: &str) -> Option<u8> {
		self.blocks.last().unwrap().get(id).copied()
	}
	
	fn find_chunk_binding(&self, id: &str) -> Option<Binding> {
		for ctx in self.blocks.iter().rev() {
			if let Some(reg) = ctx.get(id) {
				return Some(Binding::Local(*reg));
			}
		}
		if let Some((i,_)) = self.upvalues.iter().enumerate().find(|(_,u)| u.name == id) {
			return Some(Binding::Upvalue(u8::try_from(i).unwrap()));
		}
		None
	}
	
	fn make_local(&mut self, id: String, reg: u8) {
		self.blocks.last_mut().unwrap().insert(id, reg);
		self.regs.make_local(reg);
	}
	
	fn make_upvalue(&mut self, id: String, reg: u8) -> Result<u8, HissyError> {
		let upv = u8::try_from(self.upvalues.len()).map_err(|_| error_str("Too many upvalues in chunk"));
		self.upvalues.push(UpvalueBinding { name: id, reg });
		upv
	}
}


struct Context {
	stack: Vec<ChunkContext>,
}

impl Context {
	pub fn new() -> Context {
		Context { stack: Vec::new() }
	}
	
	fn enter(&mut self) {
		self.stack.push(ChunkContext::new());
	}
	
	fn leave(&mut self) {
		self.stack.pop().expect("Cannot leave main chunk");
	}
}

impl Deref for Context {
	type Target = ChunkContext;
	
	fn deref(&self) -> &ChunkContext {
		self.stack.last().unwrap()
	}
}
impl DerefMut for Context {
	fn deref_mut(&mut self) -> &mut ChunkContext {
		self.stack.last_mut().unwrap()
	}
}



struct ChunkManager {
	chunks: Vec<Chunk>,
	stack: Vec<usize>,
}

impl ChunkManager {
	fn new() -> ChunkManager {
		ChunkManager { chunks: vec![], stack: vec![] }
	}
	
	fn enter(&mut self) -> usize {
		let idx = self.chunks.len();
		self.chunks.push(Chunk::new());
		self.stack.push(idx);
		idx
	}
	fn leave(&mut self) {
		self.stack.pop().unwrap();
	}
	
	fn finish(self) -> Vec<Chunk> {
		self.chunks
	}
}

impl Deref for ChunkManager {
	type Target = Chunk;
	
	fn deref(&self) -> &Chunk {
		&self.chunks[*self.stack.last().unwrap()]
	}
}
impl DerefMut for ChunkManager {
	fn deref_mut(&mut self) -> &mut Chunk {
		&mut self.chunks[*self.stack.last().unwrap()]
	}
}


/// A struct holding state necessary to compilation.
pub struct Compiler {
	debug_info: bool,
	ctx: Context,
	chunk: ChunkManager,
}

impl Compiler {
	/// Creates a new `Compiler` object.
	pub fn new(debug_info: bool) -> Compiler {
		Compiler { debug_info, ctx: Context::new(), chunk: ChunkManager::new() }
	}
	
	fn get_binding(&mut self, id: &str) -> Result<Option<Binding>, HissyError> {
		// Find a binding (local or known upvalue) in current chunk, otherwise...
		if let Some(binding) = self.ctx.find_chunk_binding(id) {
			Ok(Some(binding))
		} else {
			// Look for a binding in surrounding chunks, and if found...
			let binding = self.ctx.stack.iter().enumerate().rev().skip(1).find_map(|(i, ctx)| {
				ctx.find_chunk_binding(id).map(|b| (i, b))
			});
			if let Some((i, mut binding)) = binding {
				// Set it as an upvalue in all inner chunks successively.
				for ctx in self.ctx.stack[i+1..].iter_mut() {
					let upv = ctx.make_upvalue(id.to_string(), binding.encoded())?;
					binding = Binding::Upvalue(upv);
				}
				Ok(Some(binding))
			} else {
				Ok(None)
			}
		}
	}
	
	// Emits register to chunk; dest if Some, else new_reg()
	fn emit_reg(&mut self, dest: Option<u8>) -> Result<u8, HissyError> {
		let reg = dest.map_or_else(|| self.ctx.regs.new_reg(), Ok)?;
		self.chunk.emit_byte(reg);
		Ok(reg)
	}
	
	// Compile computation of expr (into dest if given), and returns final register
	// Warning: If no dest is given, do not assume the final register is a new, temporary one,
	// it may be a local or a constant!
	fn compile_expr(&mut self, expr: Expr, dest: Option<u8>, name: Option<String>) -> Result<u8, HissyError> {
		let mut needs_copy = true;
		
		let mut reg = match expr {
			Expr::Nil =>
				self.chunk.compile_constant(ChunkConstant::Nil)?,
			Expr::Bool(b) =>
				self.chunk.compile_constant(ChunkConstant::Bool(b))?,
			Expr::Int(i) =>
				self.chunk.compile_constant(ChunkConstant::Int(i))?,
			Expr::Real(r) =>
				self.chunk.compile_constant(ChunkConstant::Real(r))?,
			Expr::String(s) => 
				self.chunk.compile_constant(ChunkConstant::String(s))?,
			Expr::Id(s) => {
				let binding = self.get_binding(&s)?
					.ok_or_else(|| error(format!("Referencing undefined binding '{}'", s)))?;
				match binding {
					Binding::Local(reg) => reg,
					Binding::Upvalue(upv) => {
						self.chunk.emit_instr(InstrType::GetUp);
						self.chunk.emit_byte(upv);
						needs_copy = false;
						self.emit_reg(dest)?
					},
				}
			},
			Expr::BinOp(op, e1, e2) => {
				let r1 = self.compile_expr(*e1, None, None)?;
				let r2 = self.compile_expr(*e2, None, None)?;
				self.ctx.regs.free_temp_reg(r2);
				self.ctx.regs.free_temp_reg(r1);
				let instr = match op {
					BinOp::Plus => InstrType::Add,
					BinOp::Minus => InstrType::Sub,
					BinOp::Times => InstrType::Mul,
					BinOp::Divides => InstrType::Div,
					BinOp::Modulo => InstrType::Mod,
					BinOp::Power => InstrType::Pow,
					BinOp::LEq => InstrType::Leq,
					BinOp::GEq => InstrType::Geq,
					BinOp::Less => InstrType::Lth,
					BinOp::Greater => InstrType::Gth,
					BinOp::Equal => InstrType::Eq,
					BinOp::NEq => InstrType::Neq,
					BinOp::And => InstrType::And,
					BinOp::Or => InstrType::Or,
				};
				self.chunk.emit_instr(instr);
				self.chunk.emit_byte(r1);
				self.chunk.emit_byte(r2);
				needs_copy = false;
				self.emit_reg(dest)?
			},
			Expr::UnaOp(op, e) => {
				let r = self.compile_expr(*e, dest, None)?;
				self.ctx.regs.free_temp_reg(r);
				let instr = match op {
					UnaOp::Not => InstrType::Not,
					UnaOp::Minus => InstrType::Neg,
				};
				self.chunk.emit_instr(instr);
				self.chunk.emit_byte(r);
				needs_copy = false;
				self.emit_reg(dest)?
			},
			Expr::Call(e, mut args) => {
				let func = self.compile_expr(*e, None, None)?;
				let n = u16::try_from(args.len()).map_err(|_| error_str("Too many function arguments"))?;
				let arg_range = self.ctx.regs.new_reg_range(n)?;
				for (i, arg) in args.drain(..).enumerate() {
					let rout = u8::try_from(usize::from(arg_range) + i).unwrap();
					self.compile_expr(arg, Some(rout), None)?;
				}
				self.ctx.regs.free_temp_range(arg_range, n);
				self.ctx.regs.free_temp_reg(func);
				self.chunk.emit_instr(InstrType::Call);
				self.chunk.emit_byte(func);
				self.chunk.emit_byte(arg_range);
				needs_copy = false;
				self.emit_reg(dest)?
			},
			Expr::Function(args, bl) =>  {
				let new_chunk = self.compile_chunk(name.unwrap_or_else(|| String::from("<func>")), bl, args)?;
				self.chunk.emit_instr(InstrType::Func);
				self.chunk.emit_byte(new_chunk);
				needs_copy = false;
				self.emit_reg(dest)?
			},
			#[allow(unreachable_patterns)]
			_ => unimplemented!("Unimplemented expression type: {:?}", expr),
		};
		
		if needs_copy {
			if let Some(dest) = dest {
				self.chunk.emit_instr(InstrType::Cpy);
				self.chunk.emit_byte(reg);
				self.chunk.emit_byte(dest);
				reg = dest;
			}
		}
		
		Ok(reg)
	}


	fn compile_block(&mut self, stats: Block) -> Result<(), HissyError> {
		let used_before = self.ctx.regs.used;
		
		self.ctx.enter_block();
		
		for Positioned(stat, (line, _)) in stats {
			let line = u16::try_from(line).map_err(|_| error_str("Line number too large"))?;
			if self.debug_info {
				let pos = u16::try_from(self.chunk.code.len()).unwrap(); // (The code size is already bounded by the serialization)
				self.chunk.debug_info.line_numbers.push((pos, line));
			}
			
			let compile_stat = || -> Result<(), HissyError> {
				match stat {
					Stat::ExprStat(e) => {
						let reg = self.compile_expr(e, None, None)?;
						self.ctx.regs.free_temp_reg(reg);
					},
					Stat::Let((id, _ty), e) => {
						if let Some(reg) = self.ctx.find_block_local(&id) { // if binding already exists
							self.ctx.regs.free_reg(reg);
						}
						let reg = self.ctx.regs.new_reg()?;
						self.compile_expr(e, Some(reg), Some(id.clone()))?;
						self.ctx.make_local(id, reg);
					},
					Stat::Set(id, e) => {
						let binding = self.get_binding(&id)?
							.ok_or_else(|| error(format!("Referencing undefined binding '{}'", id)))?;
						match binding {
							Binding::Local(reg) => {
								self.compile_expr(e, Some(reg), None)?;
							},
							Binding::Upvalue(upv) => {
								let reg = self.compile_expr(e, None, None)?;
								self.ctx.regs.free_temp_reg(reg);
								self.chunk.emit_instr(InstrType::SetUp);
								self.chunk.emit_byte(upv);
								self.chunk.emit_byte(reg);
							},
						}
					},
					Stat::Cond(mut branches) => {
						let mut end_jmps = vec![];
						let last_branch = branches.len() - 1;
						for (i, (cond, bl)) in branches.drain(..).enumerate() {
							let mut after_jmp = None;
							match cond {
								Cond::If(e) => {
									let cond_reg = self.compile_expr(e, None, None)?;
									
									// Jump to next branch if false
									self.ctx.regs.free_temp_reg(cond_reg);
									self.chunk.emit_instr(InstrType::Jif);
									after_jmp = Some(self.chunk.code.len());
									self.chunk.emit_byte(0); // Placeholder
									self.chunk.emit_byte(cond_reg);
									
									self.compile_block(bl)?;
									
									if i != last_branch {
										// Jump out of condition at end of block
										self.chunk.emit_instr(InstrType::Jmp);
										let from2 = self.chunk.code.len();
										self.chunk.emit_byte(0); // Placeholder 2
										end_jmps.push(from2);
									}
								},
								Cond::Else => {
									self.compile_block(bl)?;
								}
							}
							
							if let Some(from) = after_jmp {
								fill_in_jump_from(&mut self.chunk, from)?;
							}
						}
						
						// Fill in jumps to end
						for from in end_jmps {
							fill_in_jump_from(&mut self.chunk, from)?;
						}
					},
					Stat::While(e, bl) => {
						let begin = self.chunk.code.len();
						let cond_reg = self.compile_expr(e, None, None)?;
						
						self.ctx.regs.free_temp_reg(cond_reg);
						self.chunk.emit_instr(InstrType::Jif);
						let placeholder = self.chunk.code.len();
						self.chunk.emit_byte(0); // Placeholder
						self.chunk.emit_byte(cond_reg);
						
						self.compile_block(bl)?;
						
						self.chunk.emit_instr(InstrType::Jmp);
						emit_jump_to(&mut self.chunk, begin)?;
						fill_in_jump_from(&mut self.chunk, placeholder)?;
					},
					Stat::Log(e) => {
						let reg = self.compile_expr(e, None, None)?;
						self.ctx.regs.free_temp_reg(reg);
						self.chunk.emit_instr(InstrType::Log);
						self.chunk.emit_byte(reg);
					},
					Stat::Return(e) => {
						let reg = self.compile_expr(e, None, None)?;
						self.ctx.regs.free_temp_reg(reg);
						self.chunk.emit_instr(InstrType::Ret);
						self.chunk.emit_byte(reg);
					},
					#[allow(unreachable_patterns)]
					_ => return Err(error(format!("Unimplemented statement type: {:?}", stat)))
				}
				Ok(())
			};
			
			let mut res = compile_stat();
			if let Err(HissyError(ErrorType::Compilation, err, 0)) = res {
				res = Err(HissyError(ErrorType::Compilation, err, line));
			}
			res?;
		}
		
		self.ctx.leave_block();
		
		assert!(used_before == self.ctx.regs.used, "Leaked register");
		// Basic check to make sure no registers have been "leaked"
		
		Ok(())
	}


	fn compile_chunk(&mut self, name: String, ast: Block, args: Vec<String>) -> Result<u8, HissyError> {
		let chunk_id = self.chunk.enter();
		
		if self.debug_info {
			self.chunk.debug_info.name = name;
		}
		
		self.ctx.enter();
		self.ctx.enter_block();
		for id in args {
			let reg = self.ctx.regs.new_reg()?;
			self.ctx.make_local(id, reg);
		}
		self.compile_block(ast)?;
		self.ctx.leave_block();
		
		self.chunk.nb_registers = self.ctx.regs.required;
		self.chunk.upvalues = self.ctx.upvalues.iter().map(|b| b.reg).collect();
		if self.debug_info {
			self.chunk.debug_info.upvalue_names = self.ctx.upvalues.iter().map(|b| b.name.clone()).collect();
		}
		self.ctx.leave();
		self.chunk.leave();
		
		u8::try_from(chunk_id).map_err(|_| error_str("Too many chunks"))
	}
	
	/// Compiles a string slice containing Hissy code into a [`Program`], consuming the `Compiler`.
	pub fn compile_program(mut self, input: &str) -> Result<Program, HissyError> {
		let ast = parse(input)?;
		self.compile_chunk(String::from("<main>"), ast, Vec::new())?;
		Ok(Program { debug_info: self.debug_info, chunks: self.chunk.finish() })
	}
}
