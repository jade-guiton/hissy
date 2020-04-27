use std::env;
use std::fmt::{Display, Debug};
use std::fs::read_to_string;
use std::path::Path;

use hissy::parser;
use hissy::parser::{lexer::{Tokens, read_tokens}, ast::ProgramAST};
use hissy::compiler::{Program, Compiler};
use hissy::vm::{gc::GCHeap, run_program};


fn format_error<T, U: Display>(r: Result<T, U>, msg: &str) -> Result<T, String> {
	r.map_err(|e| format!("{}: {}", msg, e))
}

fn display_result<T: Display>(r: Result<T, String>) {
	println!("{}", r.map_or_else(|m| format!("❎  {}", m), |m| format!("☑  Result: {}", m)));
}

fn debug_result<T: Debug>(r: Result<T, String>) {
	println!("{}", r.map_or_else(|m| format!("❎  {}", m), |m| format!("☑  Result: {:#?}", m)));
}

fn display_error(r: Result<(), String>) {
	if let Err(e) = r {
		println!("❎  {}", e);
	}
}


fn lex(file: &str) -> Result<Tokens, String> {
	let contents = format_error(read_to_string(file), "Unable to open file")?;
	format_error(read_tokens(&contents), "Lexer error")
}

fn parse(file: &str) -> Result<ProgramAST, String> {
	let contents = format_error(read_to_string(file), "Unable to open file")?;
	format_error(parser::parse(&contents), "Parse error")
}

fn compile(file: &str) -> Result<(), String> {
	let code = format_error(read_to_string(file), "Unable to open file")?;
	let compiler = Compiler::new();
	let program = format_error(compiler.compile_program(&code), "Compile error")?;
	format_error(program.to_file(Path::new(file).with_extension("hic")), "Compile error")
}

fn list(file: &str) {
	let program = Program::from_file(file);
	println!("{}", program.disassemble());
}

fn interpret(file: &str) -> Result<(), String> {
	let code = format_error(read_to_string(file), "Unable to open file")?;
	let compiler = Compiler::new();
	let program = format_error(compiler.compile_program(&code), "Compile error")?;
	
	let mut heap = GCHeap::new();
	{
		run_program(&mut heap, &program);
	}
	heap.collect();
	Ok(())
}

fn run(file: &str) -> Result<(), String> {
	let program = Program::from_file(file);
	
	let mut heap = GCHeap::new();
	{
		run_program(&mut heap, &program);
	}
	heap.collect();
	Ok(())
}

fn main() {
	let args: Vec<String> = env::args().collect();
	if args.len() == 3 {
		match args[1].as_str() {
			"lex" => return display_result(lex(&args[2])),
			"parse" => return debug_result(parse(&args[2])),
			"compile" => return display_error(compile(&args[2])),
			"list" => return list(&args[2]),
			"interpret" => return display_error(interpret(&args[2])),
			"run" => return display_error(run(&args[2])),
			_ => println!("Unknown command {:?}", args[1])
		}
	}
	println!("Usage: hissy lex|parse|compile|list|interpret|run <file>");
}