use std::marker::PhantomData;

use crate::{
    core::{
        deferred_dispatcher_operation::{
            AddBarrier, AddBundle, AddSystem, AddSystemDesc, AddThreadLocal, AddThreadLocalDesc,
            DispatcherOperation,
        },
        ecs::prelude::{Dispatcher, DispatcherBuilder, RunNow, System, World, WorldExt},
        ArcThreadPool, RunNowDesc, SystemBundle, SystemDesc,
    },
    error::Error,
};

#[cfg(feature = "legion-ecs")]
use crate::core::{
    ecs::Component,
    legion::{
        self,
        dispatcher::{
            Dispatcher as LegionDispatcher, DispatcherBuilder as LegionDispatcherBuilder,
            IntoRelativeStage,
        },
        sync::{
            ComponentSyncer, ComponentSyncerWith, EntitiesBimapRef, ResourceSyncer, SyncDirection,
            SyncerTrait,
        },
        LegionState, LegionSyncBuilder, Runnable, Schedulable, Stage,
        SystemBundle as LegionSystemBundle, ThreadLocal, World as LegionWorld,
    },
};

#[cfg(not(feature = "legion-ecs"))]
/// Initialise trait for game data
pub trait DataInit<T> {
    /// Build game data
    fn build(self, world: &mut World) -> T;
}

#[cfg(feature = "legion-ecs")]
/// Initialise trait for game data
pub trait DataInit<T> {
    /// Build game data
    fn build(self, world: &mut World, migration_state: &mut LegionState) -> T;
}

/// Allow disposing game data with access to world.
pub trait DataDispose {
    /// Perform disposal
    fn dispose(&mut self, world: &mut World);
}

/// Default game data.
///
/// The lifetimes are for the systems inside and can be `'static` unless a system has a borrowed
/// field.
#[allow(missing_debug_implementations)]
pub struct GameData<'a, 'b> {
    pub(crate) dispatcher: Option<Dispatcher<'a, 'b>>,

    #[cfg(feature = "legion-ecs")]
    pub(crate) migration_dispatcher: LegionDispatcher,

    #[cfg(feature = "legion-ecs")]
    pub(crate) migration_sync_entities_id: legion::event::ListenerId,
}

impl<'a, 'b> GameData<'a, 'b> {
    #[cfg(not(feature = "legion-ecs"))]
    /// Create new game data
    pub fn new(dispatcher: Dispatcher<'a, 'b>) -> Self {
        GameData {
            dispatcher: Some(dispatcher),
        }
    }

    #[cfg(feature = "legion-ecs")]
    /// Create new game data
    pub fn new(
        dispatcher: Dispatcher<'a, 'b>,
        migration_dispatcher: LegionDispatcher,
        migration_sync_entities_id: legion::event::ListenerId,
    ) -> Self {
        GameData {
            dispatcher: Some(dispatcher),
            migration_dispatcher,
            migration_sync_entities_id,
        }
    }

    #[cfg(not(feature = "legion-ecs"))]
    /// Update game data
    pub fn update(&mut self, world: &World) {
        if let Some(dispatcher) = &mut self.dispatcher {
            dispatcher.dispatch(&world);
        }
    }

    #[cfg(feature = "legion-ecs")]
    /// Update game data
    pub fn update(&mut self, world: &mut World, migration_state: &mut LegionState) {}

    /// Dispose game data, dropping the dispatcher
    pub fn dispose(&mut self, mut world: &mut World) {
        if let Some(dispatcher) = self.dispatcher.take() {
            dispatcher.dispose(&mut world);
        }
    }
}

impl DataDispose for () {
    fn dispose(&mut self, _world: &mut World) {}
}

impl DataDispose for GameData<'_, '_> {
    fn dispose(&mut self, world: &mut World) {
        self.dispose(world);
    }
}

/// Builder for default game data
#[allow(missing_debug_implementations)]
#[derive(Default)]
pub struct GameDataBuilder<'a, 'b> {
    dispatcher_operations: Vec<Box<dyn DispatcherOperation<'a, 'b>>>,
    disp_builder: DispatcherBuilder<'a, 'b>,

    #[cfg(feature = "legion-ecs")]
    migration_dispatcher_builder: LegionDispatcherBuilder<'a>,

    #[cfg(feature = "legion-ecs")]
    migration_sync_builders: Vec<Box<dyn LegionSyncBuilder>>,

    #[cfg(feature = "legion-ecs")]
    migration_syncers: Vec<Box<dyn SyncerTrait>>,
}

#[cfg(feature = "legion-ecs")]
impl<'a, 'b> GameDataBuilder<'a, 'b> {
    pub fn migration_resource_sync<T: legion::systems::resource::Resource>(mut self) -> Self {
        self.migration_syncers
            .push(Box::new(ResourceSyncer::<T>::default()));

        self
    }

    pub fn migration_component_sync<T>(mut self) -> Self
    where
        T: Clone + legion::storage::Component + Component,
        T::Storage: Default,
    {
        self.migration_syncers
            .push(Box::new(ComponentSyncer::<T>::default()));
        self
    }

    pub fn migration_component_sync_with<S, L, F>(mut self, f: F)
    where
        S: Send + Sync + Component,
        <S as Component>::Storage: Default,
        L: legion::storage::Component,
        F: 'static
            + Fn(
                SyncDirection,
                EntitiesBimapRef,
                Option<&mut S>,
                Option<&mut L>,
            ) -> (Option<S>, Option<L>)
            + Send
            + Sync,
    {
        self.migration_syncers
            .push(Box::new(ComponentSyncerWith::<S, L, F>::new(f)));
    }

    pub fn migration_sync_bundle<B: LegionSyncBuilder + 'static>(mut self, syncer: B) -> Self {
        self.migration_sync_builders.push(Box::new(syncer));

        self
    }

    pub fn migration_with_thread_local<T: FnOnce(&mut LegionWorld) -> Box<dyn ThreadLocal> + 'a>(
        mut self,
        desc: T,
    ) -> Self {
        self.migration_dispatcher_builder.add_thread_local(desc);

        self
    }

    pub fn migration_with_system<
        S: IntoRelativeStage,
        T: FnOnce(&mut LegionWorld) -> Box<dyn Schedulable> + 'a,
    >(
        mut self,
        stage: S,
        desc: T,
    ) -> Self {
        self.migration_dispatcher_builder.add_system(stage, desc);

        self
    }

    pub fn migration_with_bundle<T: LegionSystemBundle + 'a>(mut self, bundle: T) -> Self {
        self.migration_dispatcher_builder.add_bundle(bundle);

        self
    }
}

impl<'a, 'b> GameDataBuilder<'a, 'b> {
    /// Inserts a barrier which assures that all systems added before the
    /// barrier are executed before the ones after this barrier.
    ///
    /// Does nothing if there were no systems added since the last call to
    /// `with_barrier()`. Thread-local systems are not affected by barriers;
    /// they're always executed at the end.
    ///
    /// # Returns
    ///
    /// This function returns GameDataBuilder after it has modified it.
    ///
    /// # Examples
    ///
    /// ~~~no_run
    /// use amethyst::derive::SystemDesc;
    /// use amethyst::core::SystemDesc;
    /// use amethyst::prelude::*;
    /// use amethyst::ecs::prelude::{System, SystemData, World};
    ///
    /// #[derive(SystemDesc)]
    /// struct NopSystem;
    /// impl<'a> System<'a> for NopSystem {
    ///     type SystemData = ();
    ///     fn run(&mut self, (): Self::SystemData) {}
    /// }
    ///
    /// // Three systems are added in this example. The "tabby cat" & "tom cat"
    /// // systems will both run in parallel. Only after both cat systems have
    /// // run is the "doggo" system permitted to run them.
    /// GameDataBuilder::default()
    ///     .with(NopSystem, "tabby cat", &[])
    ///     .with(NopSystem, "tom cat", &[])
    ///     .with_barrier()
    ///     .with(NopSystem, "doggo", &[]);
    /// ~~~
    pub fn with_barrier(mut self) -> Self {
        self.dispatcher_operations.push(Box::new(AddBarrier));
        self
    }

    /// Adds a given system.
    ///
    /// __Note:__ all dependencies must be added before you add the system.
    ///
    /// # Parameters
    ///
    /// - `system`: The system that is to be added to the game loop.
    /// - `name`: A unique string to identify the system by. This is used for
    ///         dependency tracking. This name may be empty `""` string in which
    ///         case it cannot be referenced as a dependency.
    /// - `dependencies`: A list of named system that _must_ have completed running
    ///                 before this system is permitted to run.
    ///                 This may be an empty list if there is no dependencies.
    ///
    /// # Returns
    ///
    /// This function returns GameDataBuilder after it has modified it.
    ///
    /// # Type Parameters
    ///
    /// - `S`: A type that implements the `System` trait.
    ///
    /// # Panics
    ///
    /// If two system are added that share an identical name, this function will panic.
    /// Empty names are permitted, and this function will not panic if more then two are added.
    ///
    /// If a dependency is referenced (by name), but has not previously been added this
    /// function will panic.
    ///
    /// # Examples
    ///
    /// ~~~no_run
    /// use amethyst::core::SystemDesc;
    /// use amethyst::derive::SystemDesc;
    /// use amethyst::prelude::*;
    /// use amethyst::ecs::prelude::{System, SystemData, World};
    ///
    /// #[derive(SystemDesc)]
    /// struct NopSystem;
    /// impl<'a> System<'a> for NopSystem {
    ///     type SystemData = ();
    ///     fn run(&mut self, _: Self::SystemData) {}
    /// }
    ///
    /// GameDataBuilder::default()
    ///     // This will add the "foo" system to the game loop, in this case
    ///     // the "foo" system will not depend on any systems.
    ///     .with(NopSystem, "foo", &[])
    ///     // The "bar" system will only run after the "foo" system has completed
    ///     .with(NopSystem, "bar", &["foo"])
    ///     // It is legal to register a system with an empty name
    ///     .with(NopSystem, "", &[]);
    /// ~~~
    pub fn with<S, N>(mut self, system: S, name: N, dependencies: &[N]) -> Self
    where
        S: for<'c> System<'c> + 'static + Send,
        N: Into<String> + Clone,
    {
        let name = Into::<String>::into(name);
        let dependencies = dependencies
            .iter()
            .map(Clone::clone)
            .map(Into::<String>::into)
            .collect::<Vec<String>>();
        let dispatcher_operation = Box::new(AddSystem {
            system,
            name,
            dependencies,
        }) as Box<dyn DispatcherOperation<'a, 'b> + 'static>;
        self.dispatcher_operations.push(dispatcher_operation);
        self
    }

    /// Adds a system descriptor.
    ///
    /// This differs from the [`with`] System call by deferring instantiation of the `System` to
    /// when the dispatcher is built. This allows system instatiation to access resources in the
    /// `World` if necessary.
    ///
    /// __Note:__ all dependencies must be added before you add the system.
    ///
    /// # Parameters
    ///
    /// - `system_desc`: The system that is to be added to the game loop.
    /// - `name`: A unique string to identify the system by. This is used for
    ///         dependency tracking. This name may be empty `""` string in which
    ///         case it cannot be referenced as a dependency.
    /// - `dependencies`: A list of named system that _must_ have completed running
    ///                 before this system is permitted to run.
    ///                 This may be an empty list if there is no dependencies.
    ///
    /// # Returns
    ///
    /// This function returns GameDataBuilder after it has modified it.
    ///
    /// # Type Parameters
    ///
    /// - `SD`: A type that implements the `SystemDesc` trait.
    /// - `S`: A type that implements the `System` trait.
    ///
    /// # Panics
    ///
    /// If two system are added that share an identical name, this function will panic.
    /// Empty names are permitted, and this function will not panic if more then two are added.
    ///
    /// If a dependency is referenced (by name), but has not previously been added this
    /// function will panic.
    ///
    /// # Examples
    ///
    /// ~~~no_run
    /// use amethyst::core::SystemDesc;
    /// use amethyst::derive::SystemDesc;
    /// use amethyst::prelude::*;
    /// use amethyst::ecs::prelude::{System, SystemData, World};
    ///
    /// #[derive(SystemDesc)]
    /// struct NopSystem;
    /// impl<'a> System<'a> for NopSystem {
    ///     type SystemData = ();
    ///     fn run(&mut self, _: Self::SystemData) {}
    /// }
    ///
    /// GameDataBuilder::default()
    ///     // This will add the "foo" system to the game loop, in this case
    ///     // the "foo" system will not depend on any systems.
    ///     .with_system_desc(NopSystem, "foo", &[])
    ///     // The "bar" system will only run after the "foo" system has completed
    ///     .with_system_desc(NopSystem, "bar", &["foo"])
    ///     // It is legal to register a system with an empty name
    ///     .with_system_desc(NopSystem, "", &[]);
    /// ~~~
    pub fn with_system_desc<SD, S, N>(
        mut self,
        system_desc: SD,
        name: N,
        dependencies: &[N],
    ) -> Self
    where
        SD: SystemDesc<'a, 'b, S> + 'static,
        S: for<'c> System<'c> + 'static + Send,
        N: Into<String> + Clone,
    {
        let name = Into::<String>::into(name);
        let dependencies = dependencies
            .iter()
            .map(Clone::clone)
            .map(Into::<String>::into)
            .collect::<Vec<String>>();
        let dispatcher_operation = Box::new(AddSystemDesc {
            system_desc,
            name,
            dependencies,
            marker: PhantomData::<S>,
        }) as Box<dyn DispatcherOperation<'a, 'b> + 'static>;
        self.dispatcher_operations.push(dispatcher_operation);
        self
    }

    /// Add a given thread-local system.
    ///
    /// A thread-local system is one that _must_ run on the main thread of the
    /// game. A thread-local system would be necessary typically to work
    /// around vendor APIs that have thread dependent designs; an example
    /// being OpenGL which uses a thread-local state machine to function.
    ///
    /// All thread-local systems are executed sequentially after all
    /// non-thread-local systems.
    ///
    /// # Parameters
    ///
    /// - `system`: The system that is to be added to the game loop.
    ///
    /// # Returns
    ///
    /// This function returns GameDataBuilder after it has modified it.
    ///
    /// # Type Parameters
    ///
    /// - `S`: A type that implements the `System` trait.
    ///
    /// # Examples
    ///
    /// ~~~no_run
    /// use amethyst::core::SystemDesc;
    /// use amethyst::derive::SystemDesc;
    /// use amethyst::prelude::*;
    /// use amethyst::ecs::prelude::{System, SystemData, World};
    ///
    /// #[derive(SystemDesc)]
    /// struct NopSystem;
    /// impl<'a> System<'a> for NopSystem {
    ///     type SystemData = ();
    ///     fn run(&mut self, _: Self::SystemData) {}
    /// }
    ///
    /// GameDataBuilder::default()
    ///     // the Nop system is registered here
    ///     .with_thread_local(NopSystem);
    /// ~~~
    pub fn with_thread_local<S>(mut self, system: S) -> Self
    where
        S: for<'c> RunNow<'c> + 'static,
    {
        self.dispatcher_operations
            .push(Box::new(AddThreadLocal { system }));
        self
    }

    /// Add a given thread-local system.
    ///
    /// A thread-local system is one that _must_ run on the main thread of the
    /// game. A thread-local system would be necessary typically to work
    /// around vendor APIs that have thread dependent designs; an example
    /// being OpenGL which uses a thread-local state machine to function.
    ///
    /// All thread-local systems are executed sequentially after all
    /// non-thread-local systems.
    ///
    /// # Parameters
    ///
    /// - `system`: The system that is to be added to the game loop.
    ///
    /// # Returns
    ///
    /// This function returns GameDataBuilder after it has modified it.
    ///
    /// # Type Parameters
    ///
    /// - `S`: A type that implements the `System` trait.
    ///
    /// # Examples
    ///
    /// ~~~no_run
    /// use amethyst::core::SystemDesc;
    /// use amethyst::derive::SystemDesc;
    /// use amethyst::prelude::*;
    /// use amethyst::ecs::prelude::{System, SystemData, World};
    ///
    /// #[derive(SystemDesc)]
    /// struct NopSystem;
    /// impl<'a> System<'a> for NopSystem {
    ///     type SystemData = ();
    ///     fn run(&mut self, _: Self::SystemData) {}
    /// }
    ///
    /// GameDataBuilder::default()
    ///     // the Nop system is registered here
    ///     .with_thread_local(NopSystem);
    /// ~~~
    pub fn with_thread_local_desc<SD, S>(mut self, system_desc: SD) -> Self
    where
        SD: RunNowDesc<'a, 'b, S> + 'b + 'static,
        S: for<'c> RunNow<'c> + 'static,
    {
        self.dispatcher_operations
            .push(Box::new(AddThreadLocalDesc {
                system_desc,
                marker: PhantomData::<S>,
            }));
        self
    }

    /// Add a given ECS bundle to the game loop.
    ///
    /// A bundle is a container for registering a bunch of ECS systems at once.
    ///
    /// # Parameters
    ///
    /// - `world`: The `World` that contains all resources.
    /// - `bundle`: The bundle to add.
    ///
    /// # Returns
    ///
    /// This function returns GameDataBuilder after it has modified it, this is
    /// wrapped in a `Result`.
    ///
    /// # Errors
    ///
    /// This function creates systems, which use any number of dependent crates or APIs, which
    /// could result in any number of errors.
    /// See each individual bundle for a description of the errors it could produce.
    ///
    pub fn with_bundle<B>(mut self, bundle: B) -> Result<Self, Error>
    where
        B: SystemBundle<'a, 'b> + 'static,
    {
        self.dispatcher_operations
            .push(Box::new(AddBundle { bundle }));
        Ok(self)
    }

    // /// Create a basic renderer with a single given `Pass`, and optional support for the `DrawUi` pass.
    // ///
    // /// Will set the clear color to black.
    // ///
    // /// ### Parameters:
    // ///
    // /// - `path`: Path to the `DisplayConfig` configuration file
    // /// - `pass`: The single pass in the render graph
    // /// - `with_ui`: If set to true, will add the UI render pass
    // pub fn with_basic_renderer<A, P>(self, path: A, pass: P, with_ui: bool) -> Result<Self, Error>
    // where
    //     A: AsRef<Path>,
    //     P: Pass + 'b,
    // {
    //     use crate::{
    //         config::Config,
    //         renderer::{DisplayConfig, Pipeline, RenderBundle, Stage},
    //         ui::DrawUi,
    //     };
    //     let config = DisplayConfig::load(path);
    //     if with_ui {
    //         let pipe = Pipeline::build().with_stage(
    //             Stage::with_backbuffer()
    //                 .clear_target([0.0, 0.0, 0.0, 1.0], 1.0)
    //                 .with_pass(pass)
    //                 .with_pass(DrawUi::new()),
    //         );
    //         self.with_bundle(RenderBundle::new(pipe, Some(config)))
    //     } else {
    //         let pipe = Pipeline::build().with_stage(
    //             Stage::with_backbuffer()
    //                 .clear_target([0.0, 0.0, 0.0, 1.0], 1.0)
    //                 .with_pass(pass),
    //         );
    //         self.with_bundle(RenderBundle::new(pipe, Some(config)))
    //     }
    // }
}

#[cfg(not(feature = "legion-ecs"))]
impl<'a, 'b> DataInit<GameData<'a, 'b>> for GameDataBuilder<'a, 'b> {
    fn build(self, mut world: &mut World) -> GameData<'a, 'b> {
        #[cfg(not(no_threading))]
        let pool = (*world.read_resource::<ArcThreadPool>()).clone();

        let mut dispatcher_builder = self.disp_builder;

        self.dispatcher_operations
            .into_iter()
            .try_for_each(|dispatcher_operation| {
                dispatcher_operation.exec(world, &mut dispatcher_builder)
            })
            .unwrap_or_else(|e| panic!("Failed to set up dispatcher: {}", e));

        #[cfg(not(no_threading))]
        let mut dispatcher = dispatcher_builder.with_pool(pool).build();
        #[cfg(no_threading)]
        let mut dispatcher = dispatcher_builder.build();
        dispatcher.setup(&mut world);
        GameData::new(dispatcher)
    }
}

#[cfg(feature = "legion-ecs")]
impl<'a, 'b> DataInit<GameData<'a, 'b>> for GameDataBuilder<'a, 'b> {
    fn build(self, mut world: &mut World, migration_state: &mut LegionState) -> GameData<'a, 'b> {
        #[cfg(not(no_threading))]
        let pool = (*world.read_resource::<ArcThreadPool>()).clone();
        let mut dispatcher_builder = self.disp_builder;

        self.dispatcher_operations
            .into_iter()
            .try_for_each(|dispatcher_operation| {
                dispatcher_operation.exec(world, &mut dispatcher_builder)
            })
            .unwrap_or_else(|e| panic!("Failed to set up dispatcher: {}", e));

        #[cfg(not(no_threading))]
        let mut dispatcher = dispatcher_builder.with_pool(pool).build();
        #[cfg(no_threading)]
        let mut dispatcher = dispatcher_builder.build();
        dispatcher.setup(&mut world);

        /////////////////////////////////

        // Prepare legion stuff
        let mut migration_dispatcher_builder = self.migration_dispatcher_builder;

        legion::temp::setup(world, migration_state);
        // TEMP: build the syncers
        self.migration_sync_builders
            .into_iter()
            .for_each(|mut builder| {
                builder.prepare(world, migration_state, &mut migration_dispatcher_builder);
            });

        migration_state
            .syncers
            .extend(self.migration_syncers.into_iter());

        migration_state
            .syncers
            .iter()
            .for_each(|syncer| syncer.setup(world));

        // This is safe because we never mutate the syncers portion of the array, but we need unsafe pointer magic for this
        // temprorary sync because otherwise the API becomes crap.
        unsafe {
            let state = migration_state as *mut LegionState;
            migration_state
                .syncers
                .iter()
                .for_each(|s| s.sync(world, &mut *state, SyncDirection::SpecsToLegion));
        }

        // build the dispatcher
        let migration_dispatcher = migration_dispatcher_builder.build(&mut migration_state.world);

        // This is safe because we never mutate the syncers portion of the array, but we need unsafe pointer magic for this
        // temprorary sync because otherwise the API becomes crap.
        // Sync back to specs
        unsafe {
            let state = migration_state as *mut LegionState;
            migration_state
                .syncers
                .iter()
                .for_each(|s| s.sync(world, &mut *state, SyncDirection::LegionToSpecs));
        }

        GameData::new(
            dispatcher,
            migration_dispatcher,
            migration_state.world.entity_channel().bind_listener(2048),
        )
    }
}

#[cfg(not(feature = "legion-ecs"))]
impl DataInit<()> for () {
    fn build(self, _: &mut World) {}
}

#[cfg(feature = "legion-ecs")]
impl DataInit<()> for () {
    fn build(self, _: &mut World, _: &mut LegionState) {}
}
