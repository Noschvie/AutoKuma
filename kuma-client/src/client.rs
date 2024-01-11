use super::{
    util::ResultLogger, Config, Error, Event, LoginResponse, Monitor, MonitorList, MonitorType,
    Result, Tag, TagDefinition,
};
use crate::{Notification, NotificationList};
use futures_util::FutureExt;
use itertools::Itertools;
use log::{debug, trace, warn};
use rust_socketio::{
    asynchronous::{Client as SocketIO, ClientBuilder},
    Event as SocketIOEvent, Payload,
};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::{collections::HashMap, mem, str::FromStr, sync::Arc, time::Duration};
use tokio::{runtime::Handle, sync::Mutex};

struct Ready {
    pub monitor_list: bool,
    pub notification_list: bool,
}

impl Ready {
    pub fn new() -> Self {
        Self {
            monitor_list: false,
            notification_list: false,
        }
    }

    pub fn reset(&mut self) {
        *self = Ready::new()
    }

    pub fn is_ready(&self) -> bool {
        self.monitor_list && self.notification_list
    }
}

struct Worker {
    config: Arc<Config>,
    socket_io: Arc<Mutex<Option<SocketIO>>>,
    monitors: Arc<Mutex<MonitorList>>,
    notifications: Arc<Mutex<NotificationList>>,
    is_connected: Arc<Mutex<bool>>,
    is_ready: Arc<Mutex<Ready>>,
    is_logged_in: Arc<Mutex<bool>>,
}

impl Worker {
    fn new(config: Config) -> Arc<Self> {
        Arc::new(Worker {
            config: Arc::new(config),
            socket_io: Arc::new(Mutex::new(None)),
            monitors: Default::default(),
            notifications: Default::default(),
            is_connected: Arc::new(Mutex::new(false)),
            is_ready: Arc::new(Mutex::new(Ready::new())),
            is_logged_in: Arc::new(Mutex::new(false)),
        })
    }

    async fn on_monitor_list(self: &Arc<Self>, monitor_list: MonitorList) -> Result<()> {
        *self.monitors.lock().await = monitor_list;
        self.is_ready.lock().await.monitor_list = true;

        Ok(())
    }

    async fn on_notification_list(
        self: &Arc<Self>,
        notification_list: NotificationList,
    ) -> Result<()> {
        *self.notifications.lock().await = notification_list;
        self.is_ready.lock().await.notification_list = true;

        Ok(())
    }

    async fn on_info(self: &Arc<Self>) -> Result<()> {
        *self.is_connected.lock().await = true;
        if let (Some(username), Some(password), true) = (
            &self.config.username,
            &self.config.password,
            !*self.is_logged_in.lock().await,
        ) {
            self.login(username, password, self.config.mfa_token.clone())
                .await?;
        }

        Ok(())
    }

    async fn on_auto_login(self: &Arc<Self>) -> Result<()> {
        debug!("Logged in using AutoLogin!");
        *self.is_logged_in.lock().await = true;
        Ok(())
    }

    async fn on_event(self: &Arc<Self>, event: Event, payload: Value) -> Result<()> {
        match event {
            Event::MonitorList => {
                self.on_monitor_list(serde_json::from_value(payload).unwrap())
                    .await?
            }
            Event::NotificationList => {
                self.on_notification_list(serde_json::from_value(payload).unwrap())
                    .await?
            }
            Event::Info => self.on_info().await?,
            Event::AutoLogin => self.on_auto_login().await?,
            _ => {}
        }

        Ok(())
    }

    fn extract_response<T: DeserializeOwned>(
        response: Vec<Value>,
        result_ptr: impl AsRef<str>,
        verify: bool,
    ) -> Result<T> {
        let json = json!(response);

        if verify
            && !json
                .pointer("/0/0/ok")
                .ok_or_else(|| {
                    Error::InvalidResponse(response.clone(), result_ptr.as_ref().to_owned())
                })?
                .as_bool()
                .unwrap_or_default()
        {
            let error_msg = json
                .pointer("/0/0/msg")
                .unwrap_or_else(|| &json!(null))
                .as_str()
                .unwrap_or_else(|| "Unknown error");

            return Err(Error::ServerError(error_msg.to_owned()));
        }

        json.pointer(&format!("/0/0{}", result_ptr.as_ref()))
            .and_then(|value| serde_json::from_value(value.to_owned()).ok())
            .ok_or_else(|| Error::InvalidResponse(response, result_ptr.as_ref().to_owned()))
    }

    async fn call<A, T>(
        self: &Arc<Self>,
        method: impl Into<String>,
        args: A,
        result_ptr: impl Into<String>,
        verify: bool,
    ) -> Result<T>
    where
        A: IntoIterator<Item = Value> + Send + Clone,
        T: DeserializeOwned + Send + 'static,
    {
        let method = method.into();
        let result_ptr: String = result_ptr.into();

        let method_ref = method.clone();
        let args: A = args.clone();
        let result_ptr = result_ptr.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<T>>(1);

        let lock = self.socket_io.lock().await;
        let socket_io = match &*lock {
            Some(socket_io) => socket_io,
            None => Err(Error::Disconnected)?,
        };

        socket_io
            .emit_with_ack(
                method.clone(),
                Payload::Text(args.into_iter().collect_vec()),
                Duration::from_secs_f64(self.config.call_timeout),
                move |message: Payload, _: SocketIO| {
                    debug!("call {} -> {:?}", method_ref, &message);
                    let tx = tx.clone();
                    let result_ptr = result_ptr.clone();
                    async move {
                        _ = match message {
                            Payload::Text(response) => {
                                tx.send(Self::extract_response(response, result_ptr, verify))
                                    .await
                            }
                            _ => tx.send(Err(Error::UnsupportedResponse)).await,
                        }
                    }
                    .boxed()
                },
            )
            .await
            .map_err(|e| Error::CommunicationError(e.to_string()))?;

        let result =
            tokio::time::timeout(Duration::from_secs_f64(self.config.call_timeout), rx.recv())
                .await
                .map_err(|_| Error::CallTimeout(method.clone()))?
                .ok_or_else(|| Error::CallTimeout(method))?;

        result
    }

    pub async fn login(
        self: &Arc<Self>,
        username: impl AsRef<str>,
        password: impl AsRef<str>,
        token: Option<String>,
    ) -> Result<()> {
        let result: Result<LoginResponse> = self
            .call(
                "login",
                vec![serde_json::to_value(HashMap::from([
                    ("username", json!(username.as_ref())),
                    ("password", json!(password.as_ref())),
                    ("token", json!(token)),
                ]))
                .unwrap()],
                "",
                false,
            )
            .await;

        match result {
            Ok(LoginResponse { ok: true, .. }) => {
                debug!("Logged in as {}!", username.as_ref());
                *self.is_logged_in.lock().await = true;
                Ok(())
            }
            Ok(LoginResponse {
                ok: false,
                msg: Some(msg),
                ..
            }) => Err(Error::LoginError(msg)),
            Err(e) => {
                *self.is_logged_in.lock().await = false;
                Err(e)
            }
            _ => {
                *self.is_logged_in.lock().await = false;
                Err(Error::LoginError("Unexpect login response".to_owned()))
            }
        }
        .log_warn(|e| e.to_string())
    }

    async fn get_tags(self: &Arc<Self>) -> Result<Vec<TagDefinition>> {
        self.call("getTags", vec![], "/tags", true).await
    }

    pub async fn add_tag(self: &Arc<Self>, tag: &mut TagDefinition) -> Result<()> {
        *tag = self
            .call(
                "addTag",
                vec![serde_json::to_value(tag.clone()).unwrap()],
                "/tag",
                true,
            )
            .await?;

        Ok(())
    }

    pub async fn edit_tag(self: &Arc<Self>, tag: &mut TagDefinition) -> Result<()> {
        *tag = self
            .call(
                "editTag",
                vec![serde_json::to_value(tag.clone()).unwrap()],
                "/tag",
                true,
            )
            .await?;

        Ok(())
    }

    pub async fn delete_tag(self: &Arc<Self>, tag_id: i32) -> Result<()> {
        let _: bool = self
            .call("deleteTag", vec![json!(tag_id)], "/ok", true)
            .await?;

        Ok(())
    }

    pub async fn add_notification(self: &Arc<Self>, notification: &mut Notification) -> Result<()> {
        self.edit_notification(notification).await
    }

    pub async fn edit_notification(
        self: &Arc<Self>,
        notification: &mut Notification,
    ) -> Result<()> {
        let json = serde_json::to_value(notification.clone()).unwrap();
        let config_json = serde_json::to_value(notification.config.clone()).unwrap();

        let merge = serde_merge::omerge(config_json, &json).unwrap();

        notification.id = Some(
            self.call(
                "addNotification",
                vec![merge, notification.id.into()],
                "/id",
                true,
            )
            .await?,
        );

        Ok(())
    }

    pub async fn delete_notification(self: &Arc<Self>, notification_id: i32) -> Result<()> {
        let _: bool = self
            .call(
                "deleteNotification",
                vec![json!(notification_id)],
                "/ok",
                true,
            )
            .await?;

        Ok(())
    }

    pub async fn add_monitor_tag(
        self: &Arc<Self>,
        monitor_id: i32,
        tag_id: i32,
        value: Option<String>,
    ) -> Result<()> {
        let _: bool = self
            .call(
                "addMonitorTag",
                vec![
                    json!(tag_id),
                    json!(monitor_id),
                    json!(value.unwrap_or_default()),
                ],
                "/ok",
                true,
            )
            .await?;

        Ok(())
    }

    pub async fn edit_monitor_tag(
        self: &Arc<Self>,
        monitor_id: i32,
        tag_id: i32,
        value: Option<String>,
    ) -> Result<()> {
        let _: bool = self
            .call(
                "editMonitorTag",
                vec![
                    json!(tag_id),
                    json!(monitor_id),
                    json!(value.unwrap_or_default()),
                ],
                "/ok",
                true,
            )
            .await?;

        Ok(())
    }

    pub async fn delete_monitor_tag(
        self: &Arc<Self>,
        monitor_id: i32,
        tag_id: i32,
        value: Option<String>,
    ) -> Result<()> {
        let _: bool = self
            .call(
                "deleteMonitorTag",
                vec![
                    json!(tag_id),
                    json!(monitor_id),
                    json!(value.unwrap_or_default()),
                ],
                "/ok",
                true,
            )
            .await?;

        Ok(())
    }

    pub async fn delete_monitor(self: &Arc<Self>, monitor_id: i32) -> Result<()> {
        let _: bool = self
            .call("deleteMonitor", vec![json!(monitor_id)], "/ok", true)
            .await?;

        Ok(())
    }

    async fn resolve_group(self: &Arc<Self>, monitor: &mut Monitor) -> Result<()> {
        if let Some(group_name) = monitor.common().parent_name.clone() {
            monitor.common_mut().parent_name = None;

            if let Some(Some(group_id)) = self
                .monitors
                .lock()
                .await
                .iter()
                .find(|x| {
                    x.1.monitor_type() == MonitorType::Group
                        && x.1.common().tags.iter().any(|tag| {
                            tag.name.as_ref().is_some_and(|tag| tag == "AutoKuma")
                                && tag
                                    .value
                                    .as_ref()
                                    .is_some_and(|tag_value| tag_value == &group_name)
                        })
                })
                .map(|x| x.1.common().id)
            {
                monitor.common_mut().parent = Some(group_id);
            } else {
                return Err(Error::GroupNotFound(group_name));
            }
        } else {
            monitor.common_mut().parent = None;
        }
        return Ok(());
    }

    async fn update_monitor_tags(self: &Arc<Self>, monitor_id: i32, tags: &Vec<Tag>) -> Result<()> {
        let new_tags = tags
            .iter()
            .filter_map(|tag| tag.tag_id.and_then(|id| Some((id, tag))))
            .collect::<HashMap<_, _>>();

        if let Some(monitor) = self.monitors.lock().await.get(&monitor_id.to_string()) {
            let current_tags = monitor
                .common()
                .tags
                .iter()
                .filter_map(|tag| tag.tag_id.and_then(|id| Some((id, tag))))
                .collect::<HashMap<_, _>>();

            let duplicates = monitor
                .common()
                .tags
                .iter()
                .duplicates_by(|tag| tag.tag_id)
                .filter_map(|tag| tag.tag_id.as_ref().map(|id| (id, tag)))
                .collect::<HashMap<_, _>>();

            let to_delete = current_tags
                .iter()
                .filter(|(id, _)| !new_tags.contains_key(*id) && !duplicates.contains_key(*id))
                .collect_vec();

            let to_create = new_tags
                .iter()
                .filter(|(id, _)| !current_tags.contains_key(*id))
                .collect_vec();

            let to_update = current_tags
                .keys()
                .filter_map(|id| match (current_tags.get(id), new_tags.get(id)) {
                    (Some(current), Some(new)) => Some((id, current, new)),
                    _ => None,
                })
                .collect_vec();

            for (tag_id, tag) in duplicates {
                self.delete_monitor_tag(monitor_id, *tag_id, tag.value.clone())
                    .await?;
            }

            for (tag_id, tag) in to_delete {
                self.delete_monitor_tag(monitor_id, *tag_id, tag.value.clone())
                    .await?;
            }

            for (tag_id, tag) in to_create {
                self.add_monitor_tag(monitor_id, *tag_id, tag.value.clone())
                    .await?
            }

            for (tag_id, current, new) in to_update {
                if current.value != new.value {
                    self.edit_monitor_tag(monitor_id, *tag_id, new.value.clone())
                        .await?;
                }
            }
        } else {
            for tag in tags {
                if let Some(tag_id) = tag.tag_id {
                    self.add_monitor_tag(monitor_id, tag_id, tag.value.clone())
                        .await?;
                }
            }
        }

        Ok(())
    }

    pub async fn add_monitor(self: &Arc<Self>, monitor: &mut Monitor) -> Result<()> {
        self.resolve_group(monitor).await?;

        let tags = mem::take(&mut monitor.common_mut().tags);
        let notifications = mem::take(&mut monitor.common_mut().notification_id_list);

        let id: i32 = self
            .clone()
            .call(
                "add",
                vec![serde_json::to_value(&monitor).unwrap()],
                "/monitorID",
                true,
            )
            .await?;

        monitor.common_mut().id = Some(id);
        monitor.common_mut().notification_id_list = notifications;
        monitor.common_mut().tags = tags;

        self.edit_monitor(monitor).await?;

        self.monitors
            .lock()
            .await
            .insert(id.to_string(), monitor.clone());

        Ok(())
    }

    pub async fn get_monitor(self: &Arc<Self>, monitor_id: i32) -> Result<Monitor> {
        self.call(
            "getMonitor",
            vec![serde_json::to_value(monitor_id.clone()).unwrap()],
            "/monitor",
            true,
        )
        .await
        .map_err(|e| match e {
            Error::ServerError(msg) if msg.contains("Cannot read properties of null") => {
                Error::IdNotFound("Monitor".to_owned(), monitor_id)
            }
            _ => e,
        })
    }

    pub async fn edit_monitor(self: &Arc<Self>, monitor: &mut Monitor) -> Result<()> {
        self.resolve_group(monitor).await?;

        let tags = mem::take(&mut monitor.common_mut().tags);

        let id: i32 = self
            .call(
                "editMonitor",
                vec![serde_json::to_value(&monitor).unwrap()],
                "/monitorID",
                true,
            )
            .await?;

        self.update_monitor_tags(id, &tags).await?;

        monitor.common_mut().tags = tags;

        Ok(())
    }

    pub async fn connect(self: &Arc<Self>) -> Result<()> {
        self.is_ready.lock().await.reset();
        *self.is_logged_in.lock().await = false;
        *self.socket_io.lock().await = None;

        let mut builder = ClientBuilder::new(self.config.url.clone())
            .transport_type(rust_socketio::TransportType::Websocket);

        for (key, value) in self
            .config
            .headers
            .iter()
            .filter_map(|header| header.split_once("="))
        {
            builder = builder.opening_header(key, value);
        }

        let handle = Handle::current();
        let self_ref = self.to_owned();
        let client = builder
            .on_any(move |event, payload, _| {
                let handle = handle.clone();
                let self_ref: Arc<Worker> = self_ref.clone();
                trace!("Client::on_any({:?}, {:?})", &event, &payload);
                async move {
                    match (event, payload) {
                        (SocketIOEvent::Message, Payload::Text(params)) => {
                            if let Ok(e) = Event::from_str(
                                &params[0]
                                    .as_str()
                                    .log_warn(|| "Error while deserializing Event...")
                                    .unwrap_or(""),
                            ) {
                                handle.clone().spawn(async move {
                                    _ = self_ref.clone().on_event(e, json!(null)).await.log_warn(
                                        |e| {
                                            format!(
                                                "Error while sending message event: {}",
                                                e.to_string()
                                            )
                                        },
                                    );
                                });
                            }
                        }
                        (event, Payload::Text(params)) => {
                            if let Ok(e) = Event::from_str(&String::from(event)) {
                                handle.clone().spawn(async move {
                                    _ = self_ref
                                        .clone()
                                        .on_event(e, params.into_iter().next().unwrap())
                                        .await
                                        .log_warn(|e| {
                                            format!("Error while sending event: {}", e.to_string())
                                        });
                                });
                            }
                        }
                        _ => {}
                    }
                }
                .boxed()
            })
            .connect()
            .await
            .log_error(|_| "Error during connect")
            .ok();

        debug!("Waiting for connection");

        *self.socket_io.lock().await = client;

        for i in 0..10 {
            if self.is_ready().await {
                debug!("Connected!");
                return Ok(());
            }

            debug!("Waiting for Kuma to get ready...");
            tokio::time::sleep(Duration::from_millis(200 * i)).await;
        }

        warn!("Timeout while waiting for Kuma to get ready...");
        match *self.is_connected.lock().await {
            true => Err(Error::NotAuthenticated),
            false => Err(Error::ConnectionTimeout),
        }
    }

    pub async fn disconnect(self: &Arc<Self>) -> Result<()> {
        let self_ref = self.to_owned();
        tokio::spawn(async move {
            let socket_io = self_ref.socket_io.lock().await;
            if let Some(socket_io) = &*socket_io {
                _ = socket_io.disconnect().await;
            }
            drop(socket_io);
            debug!("Connection closed!");
        });

        Ok(())
    }

    pub async fn is_ready(self: &Arc<Self>) -> bool {
        self.is_ready.lock().await.is_ready()
    }
}

pub struct Client {
    worker: Arc<Worker>,
}

impl Client {
    pub async fn connect(config: Config) -> Result<Client> {
        let worker = Worker::new(config);
        worker.connect().await?;

        Ok(Self { worker })
    }

    pub async fn get_monitors(&self) -> Result<MonitorList> {
        match self.worker.is_ready().await {
            true => Ok(self.worker.monitors.lock().await.clone()),
            false => Err(Error::NotReady),
        }
    }

    pub async fn get_monitor(&self, monitor_id: i32) -> Result<Monitor> {
        self.worker.get_monitor(monitor_id).await
    }

    pub async fn add_monitor(&self, mut monitor: Monitor) -> Result<Monitor> {
        self.worker.add_monitor(&mut monitor).await?;
        Ok(monitor)
    }

    pub async fn edit_monitor(&self, mut monitor: Monitor) -> Result<Monitor> {
        self.worker.edit_monitor(&mut monitor).await?;
        Ok(monitor)
    }

    pub async fn delete_monitor(&self, monitor_id: i32) -> Result<()> {
        self.worker.delete_monitor(monitor_id).await
    }

    pub async fn get_tags(&self) -> Result<Vec<TagDefinition>> {
        self.worker.get_tags().await
    }

    pub async fn get_tag(&self, tag_id: i32) -> Result<TagDefinition> {
        self.worker.get_tags().await.and_then(|tags| {
            tags.into_iter()
                .find(|tag| tag.tag_id == Some(tag_id))
                .ok_or_else(|| Error::IdNotFound("Tag".to_owned(), tag_id))
        })
    }

    pub async fn add_tag(&self, mut tag: TagDefinition) -> Result<TagDefinition> {
        self.worker.add_tag(&mut tag).await?;
        Ok(tag)
    }

    pub async fn edit_tag(&self, mut tag: TagDefinition) -> Result<TagDefinition> {
        self.worker.edit_tag(&mut tag).await?;
        Ok(tag)
    }

    pub async fn delete_tag(&self, tag_id: i32) -> Result<()> {
        self.worker.delete_tag(tag_id).await
    }

    pub async fn get_notifications(&self) -> Result<NotificationList> {
        match self.worker.is_ready().await {
            true => Ok(self.worker.notifications.lock().await.clone()),
            false => Err(Error::NotReady),
        }
    }

    pub async fn get_notification(&self, notification_id: i32) -> Result<Notification> {
        self.get_notifications().await.and_then(|notifications| {
            notifications
                .into_iter()
                .find(|notification| notification.id == Some(notification_id))
                .ok_or_else(|| Error::IdNotFound("Notification".to_owned(), notification_id))
        })
    }

    pub async fn add_notification(&self, mut notification: Notification) -> Result<Notification> {
        self.worker.add_notification(&mut notification).await?;
        Ok(notification)
    }

    pub async fn edit_notification(&self, mut notification: Notification) -> Result<Notification> {
        self.worker.edit_notification(&mut notification).await?;
        Ok(notification)
    }

    pub async fn delete_notification(&self, notification_id: i32) -> Result<()> {
        self.worker.delete_notification(notification_id).await
    }

    pub async fn disconnect(&self) -> Result<()> {
        self.worker.disconnect().await
    }
}