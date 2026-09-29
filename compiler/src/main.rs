mod ast;
mod check;
mod codegen;
mod diag;
mod driver;
mod fmt;
mod lexer;
mod parser;
mod source;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(driver::main(&args));
}
