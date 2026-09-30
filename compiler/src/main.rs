mod ast;
mod check;
mod cheader;
mod cjson;
mod codegen;
mod diag;
mod driver;
mod fmt;
mod lexer;
mod parser;
mod source;
mod stdlib;
mod tir;
mod types;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(driver::main(&args));
}
