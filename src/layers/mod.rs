//! The layers module

pub mod dense;
pub mod activation;

pub use crate::tensor::*;
pub use crate::tensor::error::ShapeMismatchError;
pub type Result<T> = std::result::Result<T, ShapeMismatchError>;

pub trait Layer<T: NumT> {
    fn predict(&self, input: &Tensor<T>) -> Result<Tensor<T>>;
    fn backpropagate_delta(&self, delta: &Tensor<T>) -> Result<Tensor<T>>;
    fn descend(&mut self, rate: T, delta: &Tensor<T>, a: &Tensor<T>) -> Result<()>;
}