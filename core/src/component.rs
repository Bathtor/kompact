use crossbeam::sync::MsQueue;
use std::cell::RefCell;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use uuid::Uuid;

use super::*;

pub trait CoreContainer: Send + Sync {
    fn id(&self) -> &Uuid;
    fn core(&self) -> &ComponentCore;
    fn execute(&self) -> ();

    fn actor_ref(&self) -> ActorRef;
    fn actor_path(&self) -> ActorPath;
    fn system(&self) -> KompicsSystem {
        self.core().system().clone()
    }
}

pub struct Component<C: ComponentDefinition + Sized + 'static> {
    core: ComponentCore,
    definition: Mutex<C>,
    ctrl_queue: Arc<MsQueue<<ControlPort as Port>::Request>>,
    msg_queue: Arc<MsQueue<MsgEnvelope>>,
    skip: AtomicUsize,
}

impl<C: ComponentDefinition + Sized> Component<C> {
    pub(crate) fn new(system: KompicsSystem, definition: C) -> Component<C> {
        Component {
            core: ComponentCore::with(system),
            definition: Mutex::new(definition),
            ctrl_queue: Arc::new(MsQueue::new()),
            msg_queue: Arc::new(MsQueue::new()),
            skip: AtomicUsize::new(0),
        }
    }

    pub(crate) fn enqueue_control(&self, event: <ControlPort as Port>::Request) -> () {
        self.ctrl_queue.push(event);
        match self.core.increment_work() {
            SchedulingDecision::Schedule => {
                let system = self.core.system();
                system.schedule(self.core.component());
            }
            _ => (), // nothing
        }
    }

    pub fn definition(&self) -> &Mutex<C> {
        &self.definition
    }
    pub fn definition_mut(&mut self) -> &mut C {
        self.definition.get_mut().unwrap()
    }

    pub fn on_definition<T, F>(&self, f: F) -> T
    where
        F: FnOnce(&mut C) -> T,
    {
        let mut cd = self.definition.lock().unwrap();
        f(cd.deref_mut())
    }
}

impl<CD> ActorRefFactory for CD
where
    CD: ComponentDefinition,
{
    fn actor_ref(&self) -> ActorRef {
        self.ctx().actor_ref()
    }
    fn actor_path(&self) -> ActorPath {
        self.ctx().actor_path()
    }
}

impl<CD> Dispatching for CD
where
    CD: ComponentDefinition,
{
    fn dispatcher_ref(&self) -> ActorRef {
        self.ctx().dispatcher_ref()
    }
}

pub trait ExecuteSend {
    fn execute_send(&mut self, env: SendEnvelope) -> () {
        panic!("Sent messages should go to the dispatcher! {:?}", env);
    }
}

impl<A: ActorRaw> ExecuteSend for A {}

impl<D: Dispatcher + ActorRaw> ExecuteSend for D {
    fn execute_send(&mut self, env: SendEnvelope) -> () {
        Dispatcher::receive(self, env)
    }
}

//#[inline(always)]
//fn to_receive(env: MsgEnvelope) -> ReceiveEnvelope {
//    match env {
//        MsgEnvelope::Receive(renv) => renv,
//        MsgEnvelope::Send(SendEnvelope::Cast(cenv)) => ReceiveEnvelope::Cast(cenv),
//        MsgEnvelope::Send(SendEnvelope::Msg { .. }) => {}
//    }
//}

impl<C: ComponentDefinition + ExecuteSend + Sized> CoreContainer for Component<C> {
    fn id(&self) -> &Uuid {
        &self.core.id
    }

    fn core(&self) -> &ComponentCore {
        &self.core
    }
    fn execute(&self) -> () {
        let max_events = self.core.system.throughput();
        let max_messages = self.core.system.max_messages();
        match self.definition().lock() {
            Ok(mut guard) => {
                let mut count: usize = 0;
                while let Some(event) = self.ctrl_queue.try_pop() {
                    // ignore max_events for lifecyle events
                    //println!("Executing event: {:?}", event);
                    guard.handle(event);
                    count += 1;
                }
                while count < max_messages {
                    if let Some(env) = self.msg_queue.try_pop() {
                        match env {
                            MsgEnvelope::Receive(renv) => guard.receive(renv),
                            MsgEnvelope::Send(SendEnvelope::Cast(cenv)) => {
                                let renv = ReceiveEnvelope::Cast(cenv);
                                guard.receive(renv);
                            }
                            MsgEnvelope::Send(senv @ SendEnvelope::Msg { .. }) => {
                                guard.execute_send(senv);
                            }
                        }
                        count += 1;
                    } else {
                        break;
                    }
                }
                let rem_events = max_events - count;
                if (rem_events > 0) {
                    let res = guard.execute(rem_events, self.skip.load(Ordering::Relaxed));
                    self.skip.store(res.skip, Ordering::Relaxed);
                    count = count + res.count;

                    while count < max_events {
                        if let Some(env) = self.msg_queue.try_pop() {
                            match env {
                                MsgEnvelope::Receive(renv) => guard.receive(renv),
                                MsgEnvelope::Send(SendEnvelope::Cast(cenv)) => {
                                    let renv = ReceiveEnvelope::Cast(cenv);
                                    guard.receive(renv);
                                }
                                MsgEnvelope::Send(senv @ SendEnvelope::Msg { .. }) => {
                                    guard.execute_send(senv);
                                }
                            }
                            count += 1;
                        } else {
                            break;
                        }
                    }
                }
                match self.core.decrement_work(count) {
                    SchedulingDecision::Schedule => {
                        let system = self.core.system();
                        let cc = self.core.component();
                        system.schedule(cc);
                    }
                    _ => (), // ignore
                }
            }
            _ => {
                panic!("System poisoned!"); //TODO better error handling
            }
        }
    }

    fn actor_ref(&self) -> ActorRef {
        let msgq = Arc::downgrade(&self.msg_queue);
        let cc = Arc::downgrade(&self.core.component());
        ActorRef::new(self.actor_path(), cc, msgq)
    }

    fn actor_path(&self) -> ActorPath {
        let sysref = self.system().system_path();
        ActorPath::from((sysref, CoreContainer::id(self).clone()))
    }
}

//
//pub trait Component: CoreContainer {
//    fn setup_ports(&mut self, self_component: Arc<Mutex<Self>>) -> ();
//}

pub struct ExecuteResult {
    count: usize,
    skip: usize,
}

impl ExecuteResult {
    pub fn new(count: usize, skip: usize) -> ExecuteResult {
        ExecuteResult { count, skip }
    }
}

pub struct ComponentContext {
    component: RefCell<Option<Weak<CoreContainer>>>,
}

impl ComponentContext {
    pub fn new() -> ComponentContext {
        ComponentContext {
            component: RefCell::default(),
        }
    }

    pub(crate) fn set_component(&self, c: Arc<CoreContainer>) -> () {
        *self.component.borrow_mut() = Some(Arc::downgrade(&c));
    }

    pub fn initialise<CD>(&self, c: Arc<Component<CD>>) -> ()
    where
        CD: ComponentDefinition + 'static,
    {
        let cc = c as Arc<CoreContainer>;
        self.set_component(cc);
    }

    pub fn component(&self) -> Arc<CoreContainer> {
        match *self.component.borrow() {
            Some(ref c) => match c.upgrade() {
                Some(ac) => ac,
                None => panic!("Component already deallocated!"),
            },
            None => panic!("Component improperly initialised!"),
        }
    }

    pub fn actor_ref(&self) -> ActorRef {
        self.component().actor_ref()
    }

    pub fn actor_path(&self) -> ActorPath {
        self.component().actor_ref().actor_path()
    }

    pub fn system(&self) -> KompicsSystem {
        self.component().system()
    }

    pub fn dispatcher_ref(&self) -> ActorRef {
        self.system().dispatcher_ref()
    }
}

pub trait ComponentDefinition: Provide<ControlPort> + ActorRaw
where
    Self: Sized,
{
    fn setup(&mut self, self_component: Arc<Component<Self>>) -> ();
    fn execute(&mut self, max_events: usize, skip: usize) -> ExecuteResult;
    fn ctx(&self) -> &ComponentContext;
}

pub trait Provide<P: Port + 'static> {
    fn handle(&mut self, event: P::Request) -> ();
}

pub trait Require<P: Port + 'static> {
    fn handle(&mut self, event: P::Indication) -> ();
}

pub enum SchedulingDecision {
    Schedule,
    AlreadyScheduled,
    NoWork,
}

pub struct ComponentCore {
    id: Uuid,
    system: KompicsSystem,
    work_count: AtomicUsize,
    component: RefCell<Option<Weak<CoreContainer>>>,
}

impl ComponentCore {
    pub fn with(system: KompicsSystem) -> ComponentCore {
        ComponentCore {
            id: Uuid::new_v4(),
            system,
            work_count: AtomicUsize::new(0),
            component: RefCell::default(),
        }
    }

    pub fn system(&self) -> &KompicsSystem {
        &self.system
    }

    pub(crate) fn set_component(&self, c: Arc<CoreContainer>) -> () {
        *self.component.borrow_mut() = Some(Arc::downgrade(&c));
    }

    pub fn component(&self) -> Arc<CoreContainer> {
        match *self.component.borrow() {
            Some(ref c) => match c.upgrade() {
                Some(ac) => ac,
                None => panic!("Component already deallocated!"),
            },
            None => panic!("Component improperly initialised!"),
        }
    }

    pub(crate) fn increment_work(&self) -> SchedulingDecision {
        if self.work_count.fetch_add(1, Ordering::SeqCst) == 0 {
            SchedulingDecision::Schedule
        } else {
            SchedulingDecision::AlreadyScheduled
        }
    }
    pub fn decrement_work(&self, work_done: usize) -> SchedulingDecision {
        //        let oldv: isize = match work_done_u.checked_as_num() {
        //            Some(work_done) => self.work_count.fetch_sub(work_done, Ordering::SeqCst),
        //            None => {
        //
        //            }
        //        }
        let oldv = self.work_count.fetch_sub(work_done, Ordering::SeqCst);
        let newv = oldv - work_done;
        if (newv > 0) {
            SchedulingDecision::Schedule
        } else {
            SchedulingDecision::NoWork
        }
    }
}

unsafe impl<C: ComponentDefinition + Sized> Send for Component<C> {}
unsafe impl<C: ComponentDefinition + Sized> Sync for Component<C> {}
