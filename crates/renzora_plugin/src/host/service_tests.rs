use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

fn gate() -> GenGate {
    GenGate {
        counter: Default::default(),
        at: 0,
        cancelled: Default::default(),
    }
}

unsafe extern "C" fn noop(_: *const sys::SystemCall) -> sys::SystemStatus {
    sys::SystemStatus::Ok
}

fn access(world: &mut World, services: u32) -> bevy::ecs::query::FilteredAccessSet {
    let mut system = build_dispatcher(world, vec![], vec![], noop, 0, gate(), services);
    system.initialize(world)
}

#[test]
fn service_access_conflicts_only_with_requested_mutable_services() {
    let mut world = World::new();
    world.init_resource::<Time>();
    let none = access(&mut world, 0);
    assert!(none.is_compatible(&access(&mut world, 0)));
    let bits = [
        sys::system_services::MESHES,
        sys::system_services::IMAGES,
        sys::system_services::HTTP,
        sys::system_services::REPLIES,
    ];
    for a in bits {
        let a_access = access(&mut world, a);
        assert!(a_access.is_compatible(&none));
        assert!(!a_access.is_compatible(&access(&mut world, sys::system_services::ALL)));
        for b in bits {
            assert_eq!(a_access.is_compatible(&access(&mut world, b)), a != b);
        }
    }
}

#[test]
fn ordinary_ecs_writes_still_conflict_without_services() {
    #[derive(Component)]
    struct Value(u32);
    let mut world = World::new();
    world.init_resource::<Time>();
    let id = world.register_component::<Value>();
    let mut accesses = Vec::new();
    for mode in [sys::Access::Read, sys::Access::Read, sys::Access::Write] {
        let plan = TermPlan {
            id,
            access: mode,
            marshal: Marshal::Raw,
            cell_size: size_of::<Value>(),
        };
        let mut system = build_dispatcher(&mut world, vec![vec![plan]], vec![], noop, 0, gate(), 0);
        accesses.push(system.initialize(&mut world));
    }
    assert!(accesses[0].is_compatible(&accesses[1]));
    assert!(!accesses[0].is_compatible(&accesses[2]));
    assert!(!accesses[2].is_compatible(&accesses[2]));
    assert_eq!(Value(3).0, 3);
}

struct Overlap {
    arrivals: AtomicUsize,
    overlapped: AtomicUsize,
}

unsafe extern "C" fn overlap(call: *const sys::SystemCall) -> sys::SystemStatus {
    // SAFETY: the test owns the shared atomic probe beyond schedule execution.
    let probe = unsafe { &*((*call).user.cast::<Overlap>()) };
    probe.arrivals.fetch_add(1, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(3);
    while probe.arrivals.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
        std::thread::yield_now();
    }
    if probe.arrivals.load(Ordering::SeqCst) >= 2 {
        probe.overlapped.fetch_add(1, Ordering::SeqCst);
    }
    sys::SystemStatus::Ok
}

#[test]
fn independent_dispatchers_actually_overlap_in_multithreaded_executor() {
    bevy::tasks::ComputeTaskPool::get_or_init(|| {
        bevy::tasks::TaskPoolBuilder::new().num_threads(4).build()
    });
    let probe = Box::new(Overlap {
        arrivals: AtomicUsize::new(0),
        overlapped: AtomicUsize::new(0),
    });
    let mut world = World::new();
    world.init_resource::<Time>();
    // Present resources matter: optional mutable parameters used to serialize
    // these systems even though neither entry touched a service.
    world.init_resource::<Assets<Mesh>>();
    world.init_resource::<Assets<Image>>();
    world.init_resource::<PluginHttpInbox>();
    world.init_resource::<PluginServiceReplies>();
    let mut schedule = Schedule::default();
    schedule.set_executor(bevy::ecs::schedule::MultiThreadedExecutor::new());
    for _ in 0..2 {
        schedule.add_systems(build_dispatcher(
            &mut world,
            vec![],
            vec![],
            overlap,
            probe.as_ref() as *const Overlap as usize,
            gate(),
            0,
        ));
    }
    schedule.run(&mut world);
    assert_eq!(probe.arrivals.load(Ordering::SeqCst), 2);
    assert_eq!(probe.overlapped.load(Ordering::SeqCst), 2);
}

static OBSERVED: AtomicUsize = AtomicUsize::new(usize::MAX);

unsafe extern "C" fn observe_sources(call: *const sys::SystemCall) -> sys::SystemStatus {
    // SAFETY: the host supplies a live call for this synchronous entry.
    let call = unsafe { &*call };
    let bits = (u32::from(!call.meshes.is_null()) * sys::system_services::MESHES)
        | (u32::from(!call.images.is_null()) * sys::system_services::IMAGES)
        | (u32::from(!call.http.is_null()) * sys::system_services::HTTP)
        | (u32::from(!call.replies.is_null()) * sys::system_services::REPLIES);
    OBSERVED.store(bits as usize, Ordering::SeqCst);
    sys::SystemStatus::Ok
}

unsafe extern "C" fn registration_probe(
    iface: *const sys::Interface,
    host: *mut sys::Host,
) -> sys::InitResult {
    // SAFETY: init_plugin supplies the live host and its interface for init.
    let iface = unsafe { &*iface };
    let desc = sys::SystemDesc {
        entry: observe_sources,
        schedule: sys::Schedule::Update,
        queries: std::ptr::null(),
        query_count: 0,
        resources: std::ptr::null(),
        resource_count: 0,
        user: std::ptr::null_mut(),
        flags: 0,
    };
    let requested = OBSERVED.load(Ordering::SeqCst);
    // SAFETY: desc and all its (empty) tables outlive registration.
    let status = unsafe {
        if requested == usize::MAX {
            (iface.add_system)(host, &desc)
        } else {
            (iface.add_system_with_services_v1)(host, &desc, requested as u32)
        }
    };
    if status == sys::RegisterStatus::Ok {
        sys::InitResult::Ok
    } else {
        sys::InitResult::Failed
    }
}

#[test]
fn registration_preserves_legacy_access_masks_new_calls_and_refuses_unknown_bits() {
    for requested in [usize::MAX, 0, 1, 2, 4, 8, 15, 16] {
        let mut world = World::new();
        world.init_resource::<Time>();
        world.init_resource::<Schedules>();
        OBSERVED.store(requested, Ordering::SeqCst);
        let result = init_plugin(&mut world, registration_probe);
        if requested == 16 {
            assert_eq!(result, sys::InitResult::Failed);
            assert!(world.resource::<Schedules>().get(Update).is_none());
        } else {
            assert_eq!(result, sys::InitResult::Ok);
            world.run_schedule(Update);
            let expected = if requested == usize::MAX {
                15
            } else {
                requested
            };
            assert_eq!(OBSERVED.load(Ordering::SeqCst), expected);
        }
    }
}

#[test]
fn sdk_parameters_declare_their_services_and_custom_parameters_default_to_all() {
    use crate::ecs::{ResourceParam as GuestResource, SystemParam as GuestParam};
    struct Custom;
    // SAFETY: fetch returns an owned zero-sized value and accesses no call data.
    unsafe impl GuestParam for Custom {
        fn declare(_: &mut crate::ecs::InitCtx, _: &mut crate::ecs::SystemBuilder) {}
        unsafe fn fetch(_: *const sys::SystemCall, _: &mut usize) -> Self {
            Custom
        }
    }
    assert_eq!(Custom::SERVICES, sys::system_services::ALL);
    assert_eq!(
        crate::ecs::SystemBuilder::default().services,
        sys::system_services::ALL
    );
    assert_eq!(<crate::ecs::Time as GuestResource>::SERVICES, 0);
    assert_eq!(<crate::ecs::Input as GuestResource>::SERVICES, 0);
    assert_eq!(
        <crate::ecs::Query<'_, crate::ecs::Entity> as GuestParam>::SERVICES,
        0
    );
    assert_eq!(
        <(
            crate::ecs::Commands<'_>,
            crate::ecs::Meshes<'_>,
            crate::ecs::Images<'_>
        ) as GuestParam>::SERVICES,
        3
    );
    assert_eq!(<crate::ecs::Replies<'_> as GuestParam>::SERVICES, 8);
    #[cfg(feature = "http")]
    assert_eq!(<crate::http::Http<'_> as GuestParam>::SERVICES, 4);
    #[cfg(feature = "dialog")]
    assert_eq!(<crate::dialog::Dialogs<'_> as GuestParam>::SERVICES, 8);
}

#[test]
fn sdk_registration_uses_inferred_mask_and_older_hosts_are_refused() {
    static MASK: AtomicUsize = AtomicUsize::new(usize::MAX);
    struct Probe;
    // SAFETY: fetch only inspects the live call and returns an owned ZST.
    unsafe impl crate::ecs::SystemParam for Probe {
        const SERVICES: u32 = 0;
        fn declare(_: &mut crate::ecs::InitCtx, _: &mut crate::ecs::SystemBuilder) {}
        unsafe fn fetch(call: *const sys::SystemCall, _: &mut usize) -> Self {
            // SAFETY: required by SystemParam::fetch's caller contract.
            let call = unsafe { &*call };
            MASK.store(
                usize::from(
                    call.meshes.is_null()
                        && call.images.is_null()
                        && call.http.is_null()
                        && call.replies.is_null(),
                ),
                Ordering::SeqCst,
            );
            Probe
        }
    }
    struct Plugin;
    impl crate::ecs::Plugin for Plugin {
        fn build(&self, app: &mut crate::ecs::App) {
            fn tick(_: crate::ecs::Res<crate::ecs::Time>, _: Probe) {}
            app.add_systems(sys::Schedule::Update, tick);
        }
    }
    unsafe extern "C" fn init(
        iface: *const sys::Interface,
        host: *mut sys::Host,
    ) -> sys::InitResult {
        // SAFETY: test passes a complete live interface; init_plugin supplies
        // the host on success. Older-version path returns before using host.
        unsafe { crate::__plugin_init_body!(Plugin, iface, host) }
    }
    let mut world = World::new();
    world.init_resource::<Time>();
    world.init_resource::<Schedules>();
    assert_eq!(init_plugin(&mut world, init), sys::InitResult::Ok);
    world.run_schedule(Update);
    assert_eq!(MASK.load(Ordering::SeqCst), 1);
    // SAFETY: Interface owns no allocation or drop glue; its pointers refer to
    // static data. Copying it permits a local version-negotiation fixture.
    let mut older = unsafe { std::ptr::read(&IFACE) };
    older.version_minor = 10;
    // SAFETY: complete interface; version refusal precedes host dereferencing.
    assert_eq!(
        unsafe { init(&older, std::ptr::null_mut()) },
        sys::InitResult::VersionTooOld
    );
    older.version_minor = sys::VERSION_MINOR;
    older.prefix_count = sys::INTERFACE_FIELDS;
    // SAFETY: a short prefix is refused before any registration or host access.
    assert_eq!(
        unsafe { init(&older, std::ptr::null_mut()) },
        sys::InitResult::AbiMismatch
    );
}

#[test]
fn declared_http_and_reply_services_consume_real_resources() {
    unsafe extern "C" fn consume(call: *const sys::SystemCall) -> sys::SystemStatus {
        // SAFETY: call and its advertised sources are live for this invocation;
        // destination buffers are independent stack storage of the stated size.
        unsafe {
            let call = &*call;
            let mut body = [0u8; 4];
            let mut data = [0u8; 4];
            let mut http = sys::HttpRead {
                body_capacity: body.len(),
                body: body.as_mut_ptr(),
                ..sys::HttpRead::COUNTS_ONLY
            };
            let mut reply = sys::ReplyRead {
                data_capacity: data.len(),
                data: data.as_mut_ptr(),
                ..sys::ReplyRead::COUNTS_ONLY
            };
            let http_ok = !call.http.is_null() && ((*call.http).poll)(call.http, 7, &mut http);
            let reply_ok =
                !call.replies.is_null() && ((*call.replies).poll)(call.replies, 9, 7, &mut reply);
            let ok =
                http_ok && reply_ok && http.status == 200 && body == *b"http" && data == *b"data";
            *call.user.cast::<bool>() = ok;
        }
        sys::SystemStatus::Ok
    }
    let mut world = World::new();
    world.init_resource::<Time>();
    world.insert_resource(PluginHttpInbox(vec![PluginHttpResponse {
        tag: 7,
        status: 200,
        body: "http".into(),
        chunk: None,
    }]));
    world.insert_resource(PluginServiceReplies(vec![ServiceReply {
        service: 9,
        tag: 7,
        op: 0,
        payload: b"data".to_vec(),
    }]));
    let mut observed = Box::new(false);
    let mut schedule = Schedule::default();
    schedule.add_systems(build_dispatcher(
        &mut world,
        vec![],
        vec![],
        consume,
        observed.as_mut() as *mut bool as usize,
        gate(),
        sys::system_services::HTTP | sys::system_services::REPLIES,
    ));
    schedule.run(&mut world);
    assert!(*observed);
    assert!(world.resource::<PluginHttpInbox>().0.is_empty());
    assert!(world.resource::<PluginServiceReplies>().0.is_empty());
}
