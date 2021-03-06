use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::rc::Rc;
use std::cell::RefCell;

use crate::{Service, ServiceFactory};

/// Convert `Fn(Config, &mut Service1) -> Future<Service2>` fn to a service factory
pub fn apply_cfg<F, C, T, R, S, E>(srv: T, f: F) -> ApplyConfigService<F, C, T, R, S, E>
where
    F: FnMut(C, &mut T) -> R,
    T: Service,
    R: Future<Output = Result<S, E>>,
    S: Service,
{
    ApplyConfigService {
        srv: Rc::new(RefCell::new((srv, f))),
        _t: PhantomData,
    }
}

/// Convert `Fn(Config, &mut Service1) -> Future<Service2>` fn to a service factory
///
/// Service1 get constructed from `T` factory.
pub fn apply_cfg_factory<F, C, T, R, S>(
    factory: T,
    f: F,
) -> ApplyConfigServiceFactory<F, C, T, R, S>
where
    F: FnMut(C, &mut T::Service) -> R,
    T: ServiceFactory<Config = ()>,
    T::InitError: From<T::Error>,
    R: Future<Output = Result<S, T::InitError>>,
    S: Service,
{
    ApplyConfigServiceFactory {
        srv: Rc::new(RefCell::new((factory, f))),
        _t: PhantomData,
    }
}

/// Convert `Fn(Config, &mut Server) -> Future<Service>` fn to NewService\
pub struct ApplyConfigService<F, C, T, R, S, E>
where
    F: FnMut(C, &mut T) -> R,
    T: Service,
    R: Future<Output = Result<S, E>>,
    S: Service,
{
    srv: Rc<RefCell<(T, F)>>,
    _t: PhantomData<(C, R, S)>,
}

impl<F, C, T, R, S, E> Clone for ApplyConfigService<F, C, T, R, S, E>
where
    F: FnMut(C, &mut T) -> R,
    T: Service,
    R: Future<Output = Result<S, E>>,
    S: Service,
{
    fn clone(&self) -> Self {
        ApplyConfigService {
            srv: self.srv.clone(),
            _t: PhantomData,
        }
    }
}

impl<F, C, T, R, S, E> ServiceFactory for ApplyConfigService<F, C, T, R, S, E>
where
    F: FnMut(C, &mut T) -> R,
    T: Service,
    R: Future<Output = Result<S, E>>,
    S: Service,
{
    type Config = C;
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Service = S;

    type InitError = E;
    type Future = R;

    fn new_service(&self, cfg: C) -> Self::Future {
        let (t, f) = &mut *self.srv.borrow_mut();
        f(cfg, t)
    }
}

/// Convert `Fn(&Config) -> Future<Service>` fn to NewService
pub struct ApplyConfigServiceFactory<F, C, T, R, S>
where
    F: FnMut(C, &mut T::Service) -> R,
    T: ServiceFactory<Config = ()>,
    R: Future<Output = Result<S, T::InitError>>,
    S: Service,
{
    srv: Rc<RefCell<(T, F)>>,
    _t: PhantomData<(C, R, S)>,
}

impl<F, C, T, R, S> Clone for ApplyConfigServiceFactory<F, C, T, R, S>
where
    F: FnMut(C, &mut T::Service) -> R,
    T: ServiceFactory<Config = ()>,
    R: Future<Output = Result<S, T::InitError>>,
    S: Service,
{
    fn clone(&self) -> Self {
        Self {
            srv: self.srv.clone(),
            _t: PhantomData,
        }
    }
}

impl<F, C, T, R, S> ServiceFactory for ApplyConfigServiceFactory<F, C, T, R, S>
where
    F: FnMut(C, &mut T::Service) -> R,
    T: ServiceFactory<Config = ()>,
    T::InitError: From<T::Error>,
    R: Future<Output = Result<S, T::InitError>>,
    S: Service,
{
    type Config = C;
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Service = S;

    type InitError = T::InitError;
    type Future = ApplyConfigServiceFactoryResponse<F, C, T, R, S>;

    fn new_service(&self, cfg: C) -> Self::Future {
        ApplyConfigServiceFactoryResponse {
            cfg: Some(cfg),
            store: self.srv.clone(),
            state: State::A(self.srv.borrow().0.new_service(())),
        }
    }
}

#[pin_project::pin_project]
pub struct ApplyConfigServiceFactoryResponse<F, C, T, R, S>
where
    F: FnMut(C, &mut T::Service) -> R,
    T: ServiceFactory<Config = ()>,
    T::InitError: From<T::Error>,
    R: Future<Output = Result<S, T::InitError>>,
    S: Service,
{
    cfg: Option<C>,
    store: Rc<RefCell<(T, F)>>,
    #[pin]
    state: State<T, R, S>,
}

#[pin_project::pin_project]
enum State<T, R, S>
where
    T: ServiceFactory<Config = ()>,
    T::InitError: From<T::Error>,
    R: Future<Output = Result<S, T::InitError>>,
    S: Service,
{
    A(#[pin] T::Future),
    B(T::Service),
    C(#[pin] R),
}

impl<F, C, T, R, S> Future for ApplyConfigServiceFactoryResponse<F, C, T, R, S>
where
    F: FnMut(C, &mut T::Service) -> R,
    T: ServiceFactory<Config = ()>,
    T::InitError: From<T::Error>,
    R: Future<Output = Result<S, T::InitError>>,
    S: Service,
{
    type Output = Result<S, T::InitError>;

    #[pin_project::project]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        #[project]
        match this.state.as_mut().project() {
            State::A(fut) => match fut.poll(cx)? {
                Poll::Pending => Poll::Pending,
                Poll::Ready(srv) => {
                    this.state.set(State::B(srv));
                    self.poll(cx)
                }
            },
            State::B(srv) => match srv.poll_ready(cx)? {
                Poll::Ready(_) => {
                    {
                        let (_, f) = &mut *this.store.borrow_mut();
                        let fut = f(this.cfg.take().unwrap(), srv);
                        this.state.set(State::C(fut));
                    }
                    self.poll(cx)
                }
                Poll::Pending => Poll::Pending,
            },
            State::C(fut) => fut.poll(cx),
        }
    }
}
