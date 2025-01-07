use std::time::Duration;
use tokio::task::JoinSet;
use xtra::prelude::*;

#[derive(Default, xtra::Actor)]
struct Printer {
    times: usize,
}

struct Print(String);
struct Show;

impl HandlerMut<Print> for Printer {
    type Return = ();

    async fn handle_mut(&mut self, print: Print, _ctx: &mut Context<Self>) {
        self.times += 1;
        println!("Printing {}. Printed {} times so far.", print.0, self.times);
    }
}

impl Handler<Show> for Printer {
    type Return = ();

    async fn handle(&self, _show: Show, _ctx: &mut Context<Self>) {
        println!("Showing: Printed {} times so far.", self.times);
    }
}

#[tokio::main]
async fn main() {
    let addr = xtra::spawn_tokio(Printer::default(), Mailbox::unbounded(), 10);

    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut futures = JoinSet::new();

    for _ in 0..100 {
        let addr = addr.clone();
        futures.spawn(Box::pin(async move {
            if let Err(err) = addr.send(Print("hello".to_string())).await {
                eprintln!("Printer should not be dropped: {}", err);
            }
        }));
    }

    for _ in 0..10 {
        let addr = addr.clone();
        futures.spawn(Box::pin(async move {
            if let Err(err) = addr.send(Show).await {
                eprintln!("Printer should not be dropped: {}", err);
            }
        }));
    }

    tokio::time::sleep(Duration::from_millis(1000)).await;

    let jh = addr.join();

    addr.send(Print("bye".to_string())).await.expect("gone!");

    drop(addr);
    println!("address dropped");
    jh.await;
    println!("main ended");
}
