use super::sys::BinderUintptrT;
use super::transaction::Transaction;
use tokio::sync::mpsc;

pub struct BinderObject {
    pub cookie: BinderUintptrT,
    pub(crate) rx: mpsc::Receiver<Transaction>,
}
impl BinderObject {
    pub async fn recv_transaction(&mut self) -> Option<Transaction> {
        self.rx.recv().await
    }
}
