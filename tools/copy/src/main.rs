use std::any::Any;

fn is_broken_pipe_panic(payload: &Box<dyn Any + Send>) -> bool {
    payload
        .downcast_ref::<String>()
        .is_some_and(|message| message.contains("Broken pipe"))
        || payload
            .downcast_ref::<&str>()
            .is_some_and(|message| message.contains("Broken pipe"))
}

fn main() {
    let result = std::panic::catch_unwind(copy_rs::run);
    match result {
        Ok(code) => std::process::exit(code),
        Err(payload) if is_broken_pipe_panic(&payload) => std::process::exit(0),
        Err(payload) => std::panic::resume_unwind(payload),
    }
}
