//! Day-1 spike (design 3.11): the starlark crate builds, a native `agent()` can block its engine
//! thread on a reply from a std worker thread, a native `parallel()` can call script thunks, and
//! the interpreter refuses `while` and stops unbounded recursion.

use std::cell::RefCell;

use crossbeam_channel::{unbounded, Sender};
use starlark::any::ProvidesStaticType;
use starlark::environment::{GlobalsBuilder, Module};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::list::UnpackList;
use starlark::values::{Value, ValueLike};

#[derive(ProvidesStaticType)]
struct Host {
    to_worker: Sender<(String, Sender<String>)>,
    calls: RefCell<Vec<String>>,
}

#[starlark_module]
fn host_api(builder: &mut GlobalsBuilder) {
    fn agent<'v>(#[starlark(require = pos)] prompt: &str, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
        let host = eval.extra.unwrap().downcast_ref::<Host>().unwrap();
        host.calls.borrow_mut().push(prompt.to_string());
        let (tx, rx) = unbounded();
        host.to_worker.send((prompt.to_string(), tx))?;
        // Blocks the engine thread until a std thread answers.
        let answer = rx.recv()?;
        Ok(eval.heap().alloc(answer))
    }

    fn parallel<'v>(
        #[starlark(require = pos)] thunks: UnpackList<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Vec<Value<'v>>> {
        let mut out = Vec::new();
        for t in thunks.items {
            out.push(match eval.eval_function(t, &[], &[]) {
                Ok(v) => v,
                Err(_) => Value::new_none(),
            });
        }
        Ok(out)
    }
}

fn dialect() -> Dialect {
    Dialect { enable_top_level_stmt: true, enable_f_strings: true, ..Dialect::Extended }
}

fn run(src: &str) -> Result<(String, Vec<String>), String> {
    let (tx, rx) = unbounded::<(String, Sender<String>)>();
    let worker = std::thread::spawn(move || {
        for (prompt, reply) in rx {
            let _ = reply.send(format!("done:{prompt}"));
        }
    });
    let host = Host { to_worker: tx, calls: RefCell::new(Vec::new()) };
    let out = {
        let ast = AstModule::parse("spike.star", src.to_owned(), &dialect()).map_err(|e| e.to_string())?;
        let globals = GlobalsBuilder::standard().with(host_api).build();
        let module = Module::new();
        let mut eval = Evaluator::new(&module);
        eval.extra = Some(&host);
        eval.set_max_callstack_size(64).map_err(|e| e.to_string())?;
        eval.eval_module(ast, &globals).map_err(|e| e.to_string())?;
        let main = module.get("main").ok_or("no main")?;
        let v = eval.eval_function(main, &[], &[]).map_err(|e| e.to_string())?;
        v.to_json().map_err(|e| e.to_string())?
    };
    let calls = host.calls.take();
    drop(host);
    worker.join().unwrap();
    Ok((out, calls))
}

#[test]
fn agent_blocks_on_a_worker_thread_and_parallel_calls_thunks() {
    let (out, calls) = run(
        r#"
def main():
    a = agent("one")
    xs = parallel([lambda: agent("p" + str(i)) for i in range(3)])
    ys = parallel([(lambda n: lambda: agent(f"q{n}"))(i) for i in range(2)])
    return {"a": a, "xs": xs, "ys": ys}
"#,
    )
    .unwrap();
    assert!(out.contains("done:one"), "{out}");
    // Lambdas made in a comprehension: record how they bind the loop variable.
    eprintln!("calls = {calls:?}; out = {out}");
    assert_eq!(calls[0], "one");
    assert_eq!(calls.len(), 6);
    assert!(calls[4..] == ["q0".to_string(), "q1".to_string()]);
}

#[test]
fn while_is_not_starlark_and_recursion_hits_the_stack_limit() {
    let e = run("def main():\n    while True:\n        pass\n").unwrap_err();
    assert!(e.to_lowercase().contains("while") || e.contains("Parse error"), "{e}");
    let e = run("def f():\n    return f()\ndef main():\n    return f()\n").unwrap_err();
    assert!(e.contains("stack"), "{e}");
}

#[test]
fn value_like_is_usable() {
    let (out, _) = run("def main():\n    return [1, 'x', None, True, {'k': 2.5}]\n").unwrap();
    assert_eq!(out, r#"[1,"x",null,true,{"k":2.5}]"#);
    let _ = Value::new_none().to_value();
}
