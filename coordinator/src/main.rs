#![allow(dead_code)]
#![allow(unused_variables)]
#![allow(unreachable_code)]
#![allow(clippy::diverging_sub_expression)]

use std::{
  sync::Arc,
  time::Duration,
  collections::{VecDeque, HashMap},
};

use zeroize::Zeroizing;

use ciphersuite::{group::ff::Field, Ciphersuite, Ristretto};

use serai_db::{Db, MemDb};
use serai_client::Serai;

use tokio::{sync::RwLock, time::sleep};

use ::tributary::Tributary;

mod tributary;
use crate::tributary::{TributarySpec, Transaction};

mod db;
use db::MainDb;

mod p2p;
pub use p2p::*;

pub mod processor;
use processor::Processor;

mod substrate;

#[cfg(test)]
pub mod tests;

// This is a static to satisfy lifetime expectations
lazy_static::lazy_static! {
  static ref NEW_TRIBUTARIES: Arc<RwLock<VecDeque<TributarySpec>>> = Arc::new(
    RwLock::new(VecDeque::new())
  );
}

async fn run<D: Db, Pro: Processor, P: P2p>(
  raw_db: D,
  key: Zeroizing<<Ristretto as Ciphersuite>::F>,
  p2p: P,
  mut processor: Pro,
  serai: Serai,
) {
  let add_new_tributary = |db, spec: TributarySpec| async {
    // Save it to the database
    MainDb(db).add_active_tributary(&spec);
    // Add it to the queue
    // If we reboot before this is read from the queue, the fact it was saved to the database
    // means it'll be handled on reboot
    NEW_TRIBUTARIES.write().await.push_back(spec);
  };

  // Handle new Substrate blocks
  {
    let mut substrate_db = substrate::SubstrateDb::new(raw_db.clone());
    let mut last_substrate_block = substrate_db.last_block();

    let key = key.clone();
    let mut processor = processor.clone();
    tokio::spawn(async move {
      loop {
        match substrate::handle_new_blocks(
          &mut substrate_db,
          &key,
          add_new_tributary,
          &mut processor,
          &serai,
          &mut last_substrate_block,
        )
        .await
        {
          Ok(()) => sleep(Duration::from_secs(3)).await,
          Err(e) => {
            log::error!("couldn't communicate with serai node: {e}");
            sleep(Duration::from_secs(5)).await;
          }
        }
      }
    });
  }

  // Handle the Tributaries
  {
    struct ActiveTributary<D: Db, P: P2p> {
      spec: TributarySpec,
      tributary: Tributary<D, Transaction, P>,
    }
    let tributaries = Arc::new(RwLock::new(HashMap::<[u8; 32], ActiveTributary<D, P>>::new()));

    async fn add_tributary<D: Db, P: P2p>(
      db: D,
      key: Zeroizing<<Ristretto as Ciphersuite>::F>,
      p2p: P,
      tributaries: &mut HashMap<[u8; 32], ActiveTributary<D, P>>,
      spec: TributarySpec,
    ) {
      let tributary = Tributary::<_, Transaction, _>::new(
        // TODO: Use a db on a distinct volume
        db,
        spec.genesis(),
        spec.start_time(),
        key,
        spec.validators(),
        p2p,
      )
      .await
      .unwrap();

      tributaries.insert(tributary.genesis(), ActiveTributary { spec, tributary });
    }

    // Reload active tributaries from the database
    // TODO: Can MainDb take a borrow?
    for spec in MainDb(raw_db.clone()).active_tributaries().1 {
      add_tributary(
        raw_db.clone(),
        key.clone(),
        p2p.clone(),
        &mut *tributaries.write().await,
        spec,
      )
      .await;
    }

    // Handle new Tributary blocks
    let mut tributary_db = tributary::TributaryDb::new(raw_db.clone());
    {
      let tributaries = tributaries.clone();
      let p2p = p2p.clone();
      tokio::spawn(async move {
        loop {
          // The following handle_new_blocks function may take an arbitrary amount of time
          // If registering a new tributary waited for a lock on the tributaries table, the
          // substrate scanner may wait on a lock for an arbitrary amount of time
          // By instead using the distinct NEW_TRIBUTARIES, there should be minimal
          // competition/blocking
          {
            let mut new_tributaries = NEW_TRIBUTARIES.write().await;
            while let Some(spec) = new_tributaries.pop_front() {
              add_tributary(
                raw_db.clone(),
                key.clone(),
                p2p.clone(),
                // This is a short-lived write acquisition, which is why it should be fine
                &mut *tributaries.write().await,
                spec,
              )
              .await;
            }
          }

          // Unknown-length read acquisition. This would risk screwing over the P2P process EXCEPT
          // they both use read locks. Accordingly, they can co-exist
          for ActiveTributary { spec, tributary } in tributaries.read().await.values() {
            tributary::scanner::handle_new_blocks::<_, _, P>(
              &mut tributary_db,
              &key,
              &mut processor,
              spec,
              tributary,
            )
            .await;
          }

          sleep(Duration::from_secs(3)).await;
        }
      });
    }

    // Handle P2P messages
    {
      tokio::spawn(async move {
        loop {
          let msg = p2p.receive().await;
          match msg.kind {
            P2pMessageKind::Tributary(genesis) => {
              let tributaries_read = tributaries.read().await;
              let Some(tributary) = tributaries_read.get(&genesis) else {
                log::debug!("received p2p message for unknown network");
                continue;
              };

              if tributary.tributary.handle_message(&msg.msg).await {
                P2p::broadcast(&p2p, msg.kind, msg.msg).await;
              }
            }
          }
        }
      });
    }
  }

  loop {
    // Handle all messages from processors
    todo!()
  }
}

#[tokio::main]
async fn main() {
  let db = MemDb::new(); // TODO

  let key = Zeroizing::new(<Ristretto as Ciphersuite>::F::ZERO); // TODO
  let p2p = LocalP2p::new(1).swap_remove(0); // TODO

  let processor = processor::MemProcessor::new(); // TODO

  let serai = || async {
    loop {
      let Ok(serai) = Serai::new("ws://127.0.0.1:9944").await else {
        log::error!("couldn't connect to the Serai node");
        sleep(Duration::from_secs(5)).await;
        continue
      };
      return serai;
    }
  };
  run(db, key, p2p, processor, serai().await).await
}
