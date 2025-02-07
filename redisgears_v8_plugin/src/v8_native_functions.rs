/*
 * Copyright Redis Ltd. 2018 - present
 * Licensed under your choice of the Redis Source Available License 2.0 (RSALv2) or
 * the Server Side Public License v1 (SSPLv1).
 */

use redisgears_plugin_api::redisgears_plugin_api::load_library_ctx::FunctionFlags;
use redisgears_plugin_api::redisgears_plugin_api::{
    load_library_ctx::LoadLibraryCtxInterface, load_library_ctx::RegisteredKeys,
    run_function_ctx::BackgroundRunFunctionCtxInterface, run_function_ctx::RedisClientCtxInterface,
    run_function_ctx::RemoteFunctionData, CallResult, GearsApiError, RefCellWrapper,
};

use v8_rs::v8::v8_array::V8LocalArray;
use v8_rs::v8::{
    isolate_scope::V8IsolateScope, v8_array_buffer::V8LocalArrayBuffer,
    v8_context_scope::V8ContextScope, v8_native_function_template::V8LocalNativeFunctionArgsIter,
    v8_object::V8LocalObject, v8_promise::V8PromiseState, v8_utf8::V8LocalUtf8,
    v8_value::V8LocalValue, v8_version,
};

use v8_derive::new_native_function;

use crate::v8_redisai::{get_redisai_api, get_redisai_client};

use crate::v8_backend::log;
use crate::v8_function_ctx::V8Function;
use crate::v8_notifications_ctx::V8NotificationsCtx;
use crate::v8_script_ctx::V8ScriptCtx;
use crate::v8_stream_ctx::V8StreamCtx;
use crate::{get_exception_msg, get_exception_v8_value, get_function_flags};

use std::cell::RefCell;
use std::sync::{Arc, Weak};

pub(crate) fn call_result_to_js_object<'isolate_scope, 'isolate>(
    isolate_scope: &'isolate_scope V8IsolateScope<'isolate>,
    ctx_scope: &V8ContextScope,
    res: CallResult,
    decode_responses: bool,
) -> Option<V8LocalValue<'isolate_scope, 'isolate>> {
    match res {
        CallResult::SimpleStr(s) => {
            let s = isolate_scope.new_string(&s).to_string_object();
            s.set(
                ctx_scope,
                &isolate_scope.new_string("__reply_type").to_value(),
                &isolate_scope.new_string("status").to_value(),
            );
            Some(s.to_value())
        }
        CallResult::BulkStr(s) => {
            let s = isolate_scope.new_string(&s).to_string_object();
            s.set(
                ctx_scope,
                &isolate_scope.new_string("__reply_type").to_value(),
                &isolate_scope.new_string("bulk_string").to_value(),
            );
            Some(s.to_value())
        }
        CallResult::Error(e) => {
            isolate_scope.raise_exception_str(&e);
            None
        }
        CallResult::Long(l) => Some(isolate_scope.new_long(l)),
        CallResult::Double(d) => Some(isolate_scope.new_double(d)),
        CallResult::Array(a) => {
            let mut has_error = false;
            let vals = a
                .into_iter()
                .map(|v| {
                    let res =
                        call_result_to_js_object(isolate_scope, ctx_scope, v, decode_responses);
                    if res.is_none() {
                        has_error = true;
                    }
                    res
                })
                .collect::<Vec<Option<V8LocalValue>>>();
            if has_error {
                return None;
            }

            let array = isolate_scope.new_array(
                &vals
                    .iter()
                    .map(|v| v.as_ref().unwrap())
                    .collect::<Vec<&V8LocalValue>>(),
            );
            Some(array.to_value())
        }
        CallResult::Map(m) => {
            let obj = isolate_scope.new_object();
            for (k, v) in m {
                let k_js_string = if decode_responses {
                    let s = match String::from_utf8(k) {
                        Ok(s) => s,
                        Err(_) => {
                            isolate_scope.raise_exception_str("Could not decode value as string");
                            return None;
                        }
                    };
                    isolate_scope.new_string(&s).to_value()
                } else {
                    isolate_scope.raise_exception_str("Binary map key is not supported");
                    return None;
                };
                let v_js = call_result_to_js_object(isolate_scope, ctx_scope, v, decode_responses);

                // if None return None
                v_js.as_ref()?;

                let v_js = v_js.unwrap();
                obj.set(ctx_scope, &k_js_string, &v_js);
            }
            Some(obj.to_value())
        }
        CallResult::Set(s) => {
            let set = isolate_scope.new_set();
            for v in s {
                let v_js_string = if decode_responses {
                    let s = match String::from_utf8(v) {
                        Ok(s) => s,
                        Err(_) => {
                            isolate_scope.raise_exception_str("Could not decode value as string");
                            return None;
                        }
                    };
                    isolate_scope.new_string(&s).to_value()
                } else {
                    isolate_scope.raise_exception_str("Binary set element is not supported");
                    return None;
                };
                set.add(ctx_scope, &v_js_string);
            }
            Some(set.to_value())
        }
        CallResult::Bool(b) => Some(isolate_scope.new_bool(b)),
        CallResult::BigNumber(s) => {
            let s = isolate_scope.new_string(&s).to_string_object();
            s.set(
                ctx_scope,
                &isolate_scope.new_string("__reply_type").to_value(),
                &isolate_scope.new_string("big_number").to_value(),
            );
            Some(s.to_value())
        }
        CallResult::VerbatimString((ext, s)) => {
            let s = isolate_scope.new_string(&s).to_string_object();
            s.set(
                ctx_scope,
                &isolate_scope.new_string("__reply_type").to_value(),
                &isolate_scope.new_string("verbatim").to_value(),
            );
            s.set(
                ctx_scope,
                &isolate_scope.new_string("__ext").to_value(),
                &isolate_scope.new_string(&ext).to_value(),
            );
            Some(s.to_value())
        }
        CallResult::Null => Some(isolate_scope.new_null()),
        CallResult::StringBuffer(s) => {
            if decode_responses {
                let s = match String::from_utf8(s) {
                    Ok(s) => s,
                    Err(_) => {
                        isolate_scope.raise_exception_str("Could not decode value as string");
                        return None;
                    }
                };
                let s = isolate_scope.new_string(&s).to_string_object();
                s.set(
                    ctx_scope,
                    &isolate_scope.new_string("__reply_type").to_value(),
                    &isolate_scope.new_string("bulk_string").to_value(),
                );
                Some(s.to_value())
            } else {
                Some(isolate_scope.new_array_buffer(&s).to_value())
            }
        }
    }
}

pub(crate) struct RedisClient {
    pub(crate) client: Option<Box<dyn RedisClientCtxInterface>>,
    allow_block: Option<bool>,
}

impl RedisClient {
    pub(crate) fn new() -> Self {
        Self {
            client: None,
            allow_block: Some(true),
        }
    }

    pub(crate) fn make_invalid(&mut self) {
        self.client = None;
        self.allow_block = None;
    }

    pub(crate) fn set_client(&mut self, c: Box<dyn RedisClientCtxInterface>) {
        self.client = Some(c);
    }

    pub(crate) fn set_allow_block(&mut self, allow_block: bool) {
        self.allow_block = Some(allow_block);
    }
}

fn js_value_to_remote_function_data(
    ctx_scope: &V8ContextScope,
    val: V8LocalValue,
) -> Option<RemoteFunctionData> {
    if val.is_array_buffer() {
        let array_buff = val.as_array_buffer();
        let data = array_buff.data();
        Some(RemoteFunctionData::Binary(data.to_vec()))
    } else {
        let arg_str = ctx_scope.json_stringify(&val);

        // if None return None
        arg_str.as_ref()?;

        let arg_str_utf8 = arg_str.unwrap().to_value().to_utf8().unwrap();
        Some(RemoteFunctionData::String(
            arg_str_utf8.as_str().to_string(),
        ))
    }
}

pub(crate) fn get_backgrounnd_client<'isolate_scope, 'isolate>(
    script_ctx: &Arc<V8ScriptCtx>,
    isolate_scope: &'isolate_scope V8IsolateScope<'isolate>,
    ctx_scope: &V8ContextScope<'isolate_scope, 'isolate>,
    redis_background_client: Arc<Box<dyn BackgroundRunFunctionCtxInterface>>,
) -> V8LocalObject<'isolate_scope, 'isolate> {
    let bg_client = isolate_scope.new_object();

    let redis_background_client_ref = Arc::clone(&redis_background_client);
    let script_ctx_ref = Arc::downgrade(script_ctx);
    bg_client.set_native_function(
        ctx_scope,
        "block",
        new_native_function!(move |isolate_scope, ctx_scope, f: V8LocalValue| {
            if !f.is_function() {
                return Err("Argument to 'block' must be a function".into());
            }

            let is_already_blocked = ctx_scope.get_private_data::<bool, _>(0);
            if is_already_blocked.is_some() && *is_already_blocked.unwrap() {
                return Err("Main thread is already blocked".into());
            }

            let redis_client = {
                let _unlocker = isolate_scope.new_unlocker();
                match redis_background_client_ref.lock() {
                    Ok(l) => l,
                    Err(err) => {
                        return Err(format!("Can not lock Redis, {}", err.get_msg()));
                    }
                }
            };
            let script_ctx_ref = match script_ctx_ref.upgrade() {
                Some(s) => s,
                None => {
                    return Err("Function were unregistered".into());
                }
            };

            let r_client = Arc::new(RefCell::new(RedisClient::new()));
            r_client.borrow_mut().set_client(redis_client);
            let c = get_redis_client(&script_ctx_ref, isolate_scope, ctx_scope, &r_client);

            let _block_guard = ctx_scope.set_private_data(0, &true); // indicate we are blocked

            script_ctx_ref.after_lock_gil();
            let res = f.call(ctx_scope, Some(&[&c.to_value()]));
            script_ctx_ref.before_release_gil();

            r_client.borrow_mut().make_invalid();
            Ok(res)
        }),
    );

    let redis_background_client_ref = Arc::clone(&redis_background_client);
    let script_ctx_weak_ref = Arc::downgrade(script_ctx);
    bg_client.set_native_function(ctx_scope, "run_on_key", new_native_function!(move |
        _isolate,
        ctx_scope,
        key: V8RedisCallArgs,
        remote_function_name: V8LocalUtf8,
        args: Vec<V8LocalValue>,
    | {
        let args_vec:Vec<RemoteFunctionData> = args.into_iter().map(|v| js_value_to_remote_function_data(ctx_scope, v).ok_or("Failed serializing arguments")).collect::<Result<_,_>>()?;

        let _ = script_ctx_weak_ref.upgrade().ok_or("Function were unregistered")?;

        let resolver = ctx_scope.new_resolver();
        let promise = resolver.get_promise();
        let mut resolver = resolver.to_value().persist();
        let script_ctx_weak_ref = Weak::clone(&script_ctx_weak_ref);
        redis_background_client_ref.run_on_key(key.as_bytes(), remote_function_name.as_str(), args_vec, Box::new(move |result|{
            let script_ctx = match script_ctx_weak_ref.upgrade() {
                Some(s) => s,
                None => {
                    resolver.forget();
                    log("Library was delete while not all the remote jobs were done");
                    return;
                }
            };

            script_ctx.compiled_library_api.run_on_background(Box::new(move||{
                let script_ctx = match script_ctx_weak_ref.upgrade() {
                    Some(s) => s,
                    None => {
                        resolver.forget();
                        log("Library was delete while not all the remote jobs were done");
                        return;
                    }
                };

                let isolate_scope = script_ctx.isolate.enter();
                let ctx_scope = script_ctx.ctx.enter(&isolate_scope);

                let resolver = resolver.take_local(&isolate_scope).as_resolver();
                match result {
                    Ok(r) => {
                        let v = match &r {
                            RemoteFunctionData::Binary(b) => isolate_scope.new_array_buffer(b).to_value(),
                            RemoteFunctionData::String(s) => {
                                let v8_str = isolate_scope.new_string(s);
                                let v8_obj = ctx_scope.new_object_from_json(&v8_str);
                                if v8_obj.is_none() {
                                    resolver.reject(&ctx_scope, &isolate_scope.new_string("Failed deserializing remote function result").to_value());
                                    return;
                                }
                                v8_obj.unwrap()
                            }
                        };
                        resolver.resolve(&ctx_scope, &v)
                    },
                    Err(e) => {
                        resolver.reject(&ctx_scope, &isolate_scope.new_string(e.get_msg()).to_value());
                    }
                }
            }));
        }));
        Ok::<_, &'static str>(Some(promise.to_value()))
    }));

    let redis_background_client_ref = Arc::clone(&redis_background_client);
    let script_ctx_weak_ref = Arc::downgrade(script_ctx);
    bg_client.set_native_function(ctx_scope, "run_on_all_shards", new_native_function!(move |
        _isolate,
        ctx_scope,
        remote_function_name: V8LocalUtf8,
        args: Vec<V8LocalValue>,
    | {
        let args_vec:Vec<RemoteFunctionData> = args.into_iter().map(|v| js_value_to_remote_function_data(ctx_scope, v).ok_or("Failed serializing arguments")).collect::<Result<_,_>>()?;

        let _ = match script_ctx_weak_ref.upgrade() {
            Some(s) => s,
            None => {
                return Err("Function were unregistered");
            }
        };

        let resolver = ctx_scope.new_resolver();
        let promise = resolver.get_promise();
        let mut resolver = resolver.to_value().persist();
        let script_ctx_weak_ref = Weak::clone(&script_ctx_weak_ref);
        redis_background_client_ref.run_on_all_shards(remote_function_name.as_str(), args_vec, Box::new(move |results, mut errors|{
            let script_ctx = match script_ctx_weak_ref.upgrade() {
                Some(s) => s,
                None => {
                    resolver.forget();
                    log("Library was delete while not all the remote jobs were done");
                    return;
                }
            };

            script_ctx.compiled_library_api.run_on_background(Box::new(move||{
                let script_ctx = match script_ctx_weak_ref.upgrade() {
                    Some(s) => s,
                    None => {
                        resolver.forget();
                        log("Library was delete while not all the remote jobs were done");
                        return;
                    }
                };

                let isolate_scope = script_ctx.isolate.enter();
                let ctx_scope = script_ctx.ctx.enter(&isolate_scope);

                let resolver = resolver.take_local(&isolate_scope).as_resolver();
                let results: Vec<V8LocalValue> = results.into_iter().map(|v| {
                    match v {
                        RemoteFunctionData::Binary(b) => isolate_scope.new_array_buffer(&b).to_value(),
                        RemoteFunctionData::String(s) => {
                            let v8_str = isolate_scope.new_string(&s);
                            let v8_obj = ctx_scope.new_object_from_json(&v8_str);
                            if v8_obj.is_none() {
                                errors.push(GearsApiError::new(format!("Failed deserializing remote function result '{}'", s)));
                            }
                            v8_obj.unwrap()
                        }
                    }
                }).collect();
                let errors: Vec<V8LocalValue> = errors.into_iter().map(|e| isolate_scope.new_string(e.get_msg()).to_value()).collect();
                let results_array = isolate_scope.new_array(&results.iter().collect::<Vec<&V8LocalValue>>()).to_value();
                let errors_array = isolate_scope.new_array(&errors.iter().collect::<Vec<&V8LocalValue>>()).to_value();

                resolver.resolve(&ctx_scope, &isolate_scope.new_array(&[&results_array, &errors_array]).to_value());
            }));
        }));
        Ok(Some(promise.to_value()))
    }));

    bg_client
}

enum V8RedisCallArgs<'isolate_scope, 'isolate> {
    Utf8(V8LocalUtf8<'isolate_scope, 'isolate>),
    ArrBuff(V8LocalArrayBuffer<'isolate_scope, 'isolate>),
}

impl<'isolate_scope, 'isolate> V8RedisCallArgs<'isolate_scope, 'isolate> {
    fn as_bytes(&self) -> &[u8] {
        match self {
            V8RedisCallArgs::Utf8(val) => val.as_str().as_bytes(),
            V8RedisCallArgs::ArrBuff(val) => val.data(),
        }
    }
}

impl<'isolate_scope, 'isolate> TryFrom<V8LocalValue<'isolate_scope, 'isolate>>
    for V8RedisCallArgs<'isolate_scope, 'isolate>
{
    type Error = &'static str;

    fn try_from(val: V8LocalValue<'isolate_scope, 'isolate>) -> Result<Self, Self::Error> {
        if val.is_string() || val.is_string_object() {
            match val.to_utf8() {
                Some(val) => Ok(V8RedisCallArgs::Utf8(val)),
                None => Err("Can not convert value into bytes buffer"),
            }
        } else if val.is_array_buffer() {
            Ok(V8RedisCallArgs::ArrBuff(val.as_array_buffer()))
        } else {
            Err("Can not convert value into bytes buffer")
        }
    }
}

impl<'isolate_scope, 'isolate, 'a>
    TryFrom<&mut V8LocalNativeFunctionArgsIter<'isolate_scope, 'isolate, 'a>>
    for V8RedisCallArgs<'isolate_scope, 'isolate>
{
    type Error = &'static str;

    fn try_from(
        val: &mut V8LocalNativeFunctionArgsIter<'isolate_scope, 'isolate, 'a>,
    ) -> Result<Self, Self::Error> {
        val.next().ok_or("Wrong number of arguments.")?.try_into()
    }
}

fn add_call_function(
    ctx_scope: &V8ContextScope,
    redis_client: &Arc<RefCell<RedisClient>>,
    client: &V8LocalObject,
    function_name: &str,
    decode_response: bool,
) {
    let redis_client_ref = Arc::clone(redis_client);
    client.set_native_function(
        ctx_scope,
        function_name,
        new_native_function!(
            move |isolate_scope,
                  ctx_scope,
                  command_utf8: V8LocalUtf8,
                  commands_args: Vec<V8RedisCallArgs>| {
                let is_already_blocked = ctx_scope.get_private_data::<bool, _>(0);
                if is_already_blocked.is_none() || !*is_already_blocked.unwrap() {
                    return Err("Main thread is not locked");
                }

                let res = match redis_client_ref.borrow().client.as_ref() {
                    Some(c) => c.call(
                        command_utf8.as_str(),
                        &commands_args
                            .iter()
                            .map(|v| v.as_bytes())
                            .collect::<Vec<&[u8]>>(),
                    ),
                    None => return Err("Used on invalid client"),
                };

                Ok(call_result_to_js_object(
                    isolate_scope,
                    ctx_scope,
                    res,
                    decode_response,
                ))
            }
        ),
    );
}

pub(crate) fn get_redis_client<'isolate_scope, 'isolate>(
    script_ctx: &Arc<V8ScriptCtx>,
    isolate_scope: &'isolate_scope V8IsolateScope<'isolate>,
    ctx_scope: &V8ContextScope,
    redis_client: &Arc<RefCell<RedisClient>>,
) -> V8LocalObject<'isolate_scope, 'isolate> {
    let client = isolate_scope.new_object();

    add_call_function(ctx_scope, redis_client, &client, "call", true);
    add_call_function(ctx_scope, redis_client, &client, "call_raw", false);

    let redis_client_ref = Arc::clone(redis_client);
    client.set_native_function(
        ctx_scope,
        "allow_block",
        new_native_function!(move |isolate_scope, _ctx_scope| {
            let res = match redis_client_ref.borrow().allow_block.as_ref() {
                Some(c) => *c,
                None => {
                    return Err("Used on invalid client");
                }
            };

            Ok(Some(isolate_scope.new_bool(res)))
        }),
    );

    let redisai_client = get_redisai_client(script_ctx, isolate_scope, ctx_scope, redis_client);
    client.set(
        ctx_scope,
        &isolate_scope.new_string("redisai").to_value(),
        &redisai_client,
    );

    let script_ctx_ref = Arc::downgrade(script_ctx);
    let redis_client_ref = Arc::clone(redis_client);
    client.set_native_function(
        ctx_scope,
        "run_on_background",
        new_native_function!(move |_isolate, ctx_scope, f: V8LocalValue| {
            let bg_redis_client = match redis_client_ref.borrow().client.as_ref() {
                Some(c) => c.get_background_redis_client(),
                None => {
                    return Err("Called 'run_on_background' out of context");
                }
            };

            if !f.is_async_function() {
                return Err("First argument to 'run_on_background' must be an async function");
            }

            let script_ctx_ref = match script_ctx_ref.upgrade() {
                Some(s) => s,
                None => {
                    return Err("Use of invalid function context");
                }
            };
            let mut f = f.persist();
            let new_script_ctx_ref = Arc::clone(&script_ctx_ref);
            let resolver = ctx_scope.new_resolver();
            let promise = resolver.get_promise();
            let mut resolver = resolver.to_value().persist();
            script_ctx_ref
                .compiled_library_api
                .run_on_background(Box::new(move || {
                    let isolate_scope = new_script_ctx_ref.isolate.enter();
                    let ctx_scope = new_script_ctx_ref.ctx.enter(&isolate_scope);
                    let trycatch = isolate_scope.new_try_catch();

                    let background_client = get_backgrounnd_client(
                        &new_script_ctx_ref,
                        &isolate_scope,
                        &ctx_scope,
                        Arc::new(bg_redis_client),
                    );
                    let res = f
                        .take_local(&isolate_scope)
                        .call(&ctx_scope, Some(&[&background_client.to_value()]));

                    let resolver = resolver.take_local(&isolate_scope).as_resolver();
                    match res {
                        Some(r) => {
                            resolver.resolve(&ctx_scope, &r);
                        }
                        None => {
                            let error_utf8 = get_exception_v8_value(
                                &new_script_ctx_ref.isolate,
                                &isolate_scope,
                                trycatch,
                            );
                            resolver.reject(&ctx_scope, &error_utf8);
                        }
                    }
                }));
            Ok(Some(promise.to_value()))
        }),
    );
    client
}

pub(crate) fn initialize_globals(
    script_ctx: &Arc<V8ScriptCtx>,
    globals: &V8LocalObject,
    isolate_scope: &V8IsolateScope,
    ctx_scope: &V8ContextScope,
    config: Option<&String>,
) -> Result<(), GearsApiError> {
    let redis = isolate_scope.new_object();

    match config {
        Some(c) => {
            let string = isolate_scope.new_string(c);
            let trycatch = isolate_scope.new_try_catch();
            let config_json = ctx_scope.new_object_from_json(&string);
            if config_json.is_none() {
                return Err(get_exception_msg(&script_ctx.isolate, trycatch, ctx_scope));
            }
            redis.set(
                ctx_scope,
                &isolate_scope.new_string("config").to_value(),
                &config_json.unwrap(),
            )
        }
        None => {
            // setting empty config
            redis.set(
                ctx_scope,
                &isolate_scope.new_string("config").to_value(),
                &isolate_scope.new_object().to_value(),
            )
        }
    }

    let script_ctx_ref = Arc::downgrade(script_ctx);
    redis.set_native_function(ctx_scope, "register_stream_consumer", new_native_function!(move|
        _isolate_scope,
        curr_ctx_scope,
        registration_name_utf8: V8LocalUtf8,
        prefix: V8LocalValue,
        window: i64,
        trim: bool,
        function_callback: V8LocalValue,
    | {
        if !function_callback.is_function() {
            return Err("Fith argument to 'register_stream_consumer' must be a function".into());
        }
        let persisted_function = function_callback.persist();

        let load_ctx = curr_ctx_scope.get_private_data_mut::<&mut dyn LoadLibraryCtxInterface, _>(0);
        if load_ctx.is_none() {
            return Err("Called 'register_function' out of context".into());
        }
        let load_ctx = load_ctx.unwrap();

        let script_ctx_ref = match script_ctx_ref.upgrade() {
            Some(s) => s,
            None => {
                return Err("Use of uninitialized script context".into());
            }
        };

        let v8_stream_ctx = V8StreamCtx::new(persisted_function, &script_ctx_ref, function_callback.is_async_function());
        let res = if prefix.is_string() {
            let prefix = prefix.to_utf8().unwrap();
            load_ctx.register_stream_consumer(registration_name_utf8.as_str(), prefix.as_str().as_bytes(), Box::new(v8_stream_ctx), window as usize, trim)
        } else if prefix.is_array_buffer() {
            let prefix = prefix.as_array_buffer();
            load_ctx.register_stream_consumer(registration_name_utf8.as_str(), prefix.data(), Box::new(v8_stream_ctx), window as usize, trim)
        } else {
            return Err("Second argument to 'register_stream_consumer' must be a String or ArrayBuffer representing the prefix".into());
        };
        if let Err(err) = res {
            return Err(err.get_msg().to_string());
        }
        Ok(None)
    }));

    let script_ctx_ref = Arc::downgrade(script_ctx);
    redis.set_native_function(ctx_scope, "register_notifications_consumer", new_native_function!(move|
        _isolate_scope,
        curr_ctx_scope,
        registration_name_utf8: V8LocalUtf8,
        prefix: V8LocalValue,
        function_callback: V8LocalValue,
    | {
        if !function_callback.is_function() {
            return Err("Third argument to 'register_notifications_consumer' must be a function".into());
        }
        let persisted_function = function_callback.persist();

        let load_ctx = curr_ctx_scope.get_private_data_mut::<&mut dyn LoadLibraryCtxInterface, _>(0);
        if load_ctx.is_none() {
            return Err("Called 'register_notifications_consumer' out of context".into());
        }
        let load_ctx = load_ctx.unwrap();

        let script_ctx_ref = match script_ctx_ref.upgrade() {
            Some(s) => s,
            None => {
                return Err("Use of uninitialized script context".into());
            }
        };
        let v8_notification_ctx = V8NotificationsCtx::new(persisted_function, &script_ctx_ref, function_callback.is_async_function());

        let res = if prefix.is_string() {
            let prefix = prefix.to_utf8().unwrap();
            load_ctx.register_key_space_notification_consumer(registration_name_utf8.as_str(), RegisteredKeys::Prefix(prefix.as_str().as_bytes()), Box::new(v8_notification_ctx))
        } else if prefix.is_array_buffer() {
            let prefix = prefix.as_array_buffer();
            load_ctx.register_key_space_notification_consumer(registration_name_utf8.as_str(), RegisteredKeys::Prefix(prefix.data()), Box::new(v8_notification_ctx))
        } else {
            return Err("Second argument to 'register_notifications_consumer' must be a string or ArrayBuffer representing the prefix".into());
        };
        if let Err(err) = res {
            return Err(err.get_msg().to_string());
        }
        Ok(None)
    }));

    let script_ctx_ref = Arc::downgrade(script_ctx);
    redis.set_native_function(
        ctx_scope,
        "register_function",
        new_native_function!(
            move |isolate_scope,
                  curr_ctx_scope,
                  function_name_utf8: V8LocalUtf8,
                  function_callback: V8LocalValue,
                  function_flags: Option<V8LocalArray>| {
                if !function_callback.is_function() {
                    return Err(
                        "Second argument to 'register_function' must be a function".to_owned()
                    );
                }
                let persisted_function = function_callback.persist();

                let function_flags = match function_flags {
                    Some(function_flags) => get_function_flags(curr_ctx_scope, &function_flags)
                        .map_err(|e| format!("Failed parsing function flags, {}", e))?,
                    None => FunctionFlags::empty(),
                };

                let load_ctx =
                    curr_ctx_scope.get_private_data_mut::<&mut dyn LoadLibraryCtxInterface, _>(0);
                if load_ctx.is_none() {
                    return Err("Called 'register_function' out of context".into());
                }

                let script_ctx_ref = match script_ctx_ref.upgrade() {
                    Some(s) => s,
                    None => {
                        return Err("Use of uninitialized script context".into());
                    }
                };

                let load_ctx = load_ctx.unwrap();
                let c = Arc::new(RefCell::new(RedisClient::new()));
                let redis_client =
                    get_redis_client(&script_ctx_ref, isolate_scope, curr_ctx_scope, &c);

                let f = V8Function::new(
                    &script_ctx_ref,
                    persisted_function,
                    redis_client.to_value().persist(),
                    &c,
                    function_callback.is_async_function(),
                    !function_flags.contains(FunctionFlags::RAW_ARGUMENTS),
                );

                let res = load_ctx.register_function(
                    function_name_utf8.as_str(),
                    Box::new(f),
                    function_flags,
                );
                if let Err(err) = res {
                    return Err(err.get_msg().into());
                }
                Ok(None)
            }
        ),
    );

    let script_ctx_ref = Arc::downgrade(script_ctx);
    redis.set_native_function(ctx_scope, "register_remote_function", new_native_function!(move|
        _isolate_scope,
        curr_ctx_scope,
        function_name_utf8: V8LocalUtf8,
        function_callback: V8LocalValue,
    | {
        if !function_callback.is_function() {
            return Err("Second argument to 'register_remote_function' must be a function".into());
        }

        if !function_callback.is_async_function() {
            return Err("Remote function must be async".into());
        }

        let load_ctx = curr_ctx_scope.get_private_data_mut::<&mut dyn LoadLibraryCtxInterface, _>(0);
        if load_ctx.is_none() {
            return Err("Called 'register_remote_function' out of context".into());
        }

        let mut persisted_function = function_callback.persist();
        persisted_function.forget();
        let persisted_function = Arc::new(persisted_function);

        let load_ctx = load_ctx.unwrap();
        let new_script_ctx_ref = Weak::clone(&script_ctx_ref);
        let res = load_ctx.register_remote_task(function_name_utf8.as_str(), Box::new(move |inputs, background_ctx, on_done|{
            let script_ctx = match new_script_ctx_ref.upgrade() {
                Some(s) => s,
                None => {
                    on_done(Err(GearsApiError::new("Use of uninitialized script context".to_string())));
                    return;
                }
            };

            let new_script_ctx_ref = Weak::clone(&new_script_ctx_ref);
            let weak_function = Arc::downgrade(&persisted_function);
            script_ctx.compiled_library_api.run_on_background(Box::new(move || {
                let script_ctx = match new_script_ctx_ref.upgrade() {
                    Some(s) => s,
                    None => {
                        on_done(Err(GearsApiError::new("Use of uninitialized script context".to_string())));
                        return;
                    }
                };
                let persisted_function = match weak_function.upgrade() {
                    Some(s) => s,
                    None => {
                        on_done(Err(GearsApiError::new("Use of uninitialized function context".to_string())));
                        return;
                    }
                };
                let isolate_scope = script_ctx.isolate.enter();
                let ctx_scope = script_ctx.ctx.enter(&isolate_scope);
                let trycatch = isolate_scope.new_try_catch();

                let mut args = Vec::new();
                args.push(get_backgrounnd_client(&script_ctx, &isolate_scope, &ctx_scope, Arc::new(background_ctx)).to_value());
                for input in inputs {
                    args.push(match input {
                        RemoteFunctionData::Binary(b) => isolate_scope.new_array_buffer(&b).to_value(),
                        RemoteFunctionData::String(s) => {
                            let v8_str = isolate_scope.new_string(&s);
                            let v8_obj = ctx_scope.new_object_from_json(&v8_str);
                            if v8_obj.is_none() {
                                on_done(Err(GearsApiError::new("Failed deserializing remote function argument".to_string())));
                                return;
                            }
                            v8_obj.unwrap()
                        }
                    });
                }
                let args_refs = args.iter().collect::<Vec<&V8LocalValue>>();

                script_ctx.before_run();
                let res = persisted_function
                    .as_local(&isolate_scope)
                    .call(
                        &ctx_scope,
                        Some(&args_refs),
                    );
                script_ctx.after_run();
                match res {
                    Some(r) => {
                        if r.is_promise() {
                            let res = r.as_promise();
                            if res.state() == V8PromiseState::Fulfilled
                                || res.state() == V8PromiseState::Rejected
                            {
                                let r = res.get_result();
                                if res.state() == V8PromiseState::Fulfilled {
                                    let r = js_value_to_remote_function_data(&ctx_scope, r);
                                    if let Some(v) = r {
                                        on_done(Ok(v));
                                    } else {
                                        let error_utf8 = trycatch.get_exception().to_utf8().unwrap();
                                        on_done(Err(GearsApiError::new(format!("Failed serializing result, {}.", error_utf8.as_str()))));
                                    }
                                } else {
                                    let r = r.to_utf8().unwrap();
                                    on_done(Err(GearsApiError::new(r.as_str().to_string())));
                                }
                            } else {
                                // Notice, we are allowed to do this trick because we are protected by the isolate GIL
                                let done_resolve = Arc::new(RefCellWrapper{ref_cell: RefCell::new(Some(on_done))});
                                let done_reject = Arc::clone(&done_resolve);
                                let resolve =
                                    ctx_scope.new_native_function(new_native_function!(move |isolate_scope, ctx_scope, arg: V8LocalValue| {
                                        {
                                            if done_resolve.ref_cell.borrow().is_none() {
                                                return Ok::<_, String>(None)
                                            }
                                        }
                                        let on_done = done_resolve.ref_cell.borrow_mut().take().unwrap();
                                        let trycatch = isolate_scope.new_try_catch();
                                        let r = js_value_to_remote_function_data(ctx_scope, arg);
                                        if let Some(v) = r {
                                            on_done(Ok(v));
                                        } else {
                                            let error_utf8 = trycatch.get_exception().to_utf8().unwrap();
                                            on_done(Err(GearsApiError::new(format!("Failed serializing result, {}.", error_utf8.as_str()))));
                                        }
                                        Ok(None)
                                    }));
                                let reject =
                                    ctx_scope.new_native_function(new_native_function!(move |_isolate_scope, _ctx_scope, utf8_str: V8LocalUtf8| {
                                        {
                                            if done_reject.ref_cell.borrow().is_none() {
                                                return Ok::<_, String>(None);
                                            }
                                        }
                                        let on_done = done_reject.ref_cell.borrow_mut().take().unwrap();

                                        on_done(Err(GearsApiError::new(utf8_str.as_str().to_string())));
                                        Ok(None)
                                    }));
                                res.then(&ctx_scope, &resolve, &reject);
                            }
                        } else {
                            let r = js_value_to_remote_function_data(&ctx_scope, r);
                            if let Some(v) = r {
                                on_done(Ok(v));
                            } else {
                                on_done(Err(GearsApiError::new("Failed serializing result".to_string())));
                            }
                        }
                    }
                    None => {
                        let error_msg = get_exception_msg(&script_ctx.isolate, trycatch, &ctx_scope);
                        on_done(Err(error_msg));
                    }
                };
            }));
        }));

        if let Err(err) = res {
            return Err(err.get_msg().to_string());
        }
        Ok(None)
    }));

    redis.set_native_function(
        ctx_scope,
        "v8_version",
        new_native_function!(move |isolate_scope, _curr_ctx_scope| {
            let v = v8_version();
            let v_v8_str = isolate_scope.new_string(v);
            Ok::<Option<V8LocalValue>, String>(Some(v_v8_str.to_value()))
        }),
    );

    let script_ctx_ref = Arc::downgrade(script_ctx);
    redis.set_native_function(
        ctx_scope,
        "log",
        new_native_function!(move |_isolate, _curr_ctx_scope, msg: V8LocalUtf8| {
            match script_ctx_ref.upgrade() {
                Some(s) => s.compiled_library_api.log(msg.as_str()),
                None => crate::v8_backend::log(msg.as_str()), /* do not abort logs */
            }
            Ok::<Option<V8LocalValue>, String>(None)
        }),
    );

    let redis_ai = get_redisai_api(script_ctx, isolate_scope, ctx_scope);
    redis.set(
        ctx_scope,
        &isolate_scope.new_string("redisai").to_value(),
        &redis_ai,
    );

    globals.set(
        ctx_scope,
        &isolate_scope.new_string("redis").to_value(),
        &redis.to_value(),
    );

    let script_ctx_ref = Arc::downgrade(script_ctx);
    globals.set_native_function(
        ctx_scope,
        "Promise",
        new_native_function!(
            move |_isolate_scope, curr_ctx_scope, function: V8LocalValue| {
                if !function.is_function() || function.is_async_function() {
                    return Err("Bad argument to 'Promise' function");
                }

                let script_ctx_ref = script_ctx_ref
                    .upgrade()
                    .ok_or("Use of uninitialized script context")?;

                let script_ctx_ref_resolve = Arc::downgrade(&script_ctx_ref);
                let script_ctx_ref_reject = Arc::downgrade(&script_ctx_ref);
                let resolver = curr_ctx_scope.new_resolver();
                let promise = resolver.get_promise();
                let resolver_resolve = Arc::new(RefCellWrapper {
                    ref_cell: RefCell::new(resolver.to_value().persist()),
                });
                let resolver_reject = Arc::clone(&resolver_resolve);

                let resolve = curr_ctx_scope.new_native_function(new_native_function!(
                    move |_isolate, _curr_ctx_scope, arg: V8LocalValue| {
                        let script_ctx_ref_resolve = match script_ctx_ref_resolve.upgrade() {
                            Some(s) => s,
                            None => {
                                resolver_resolve.ref_cell.borrow_mut().forget();
                                return Err("Library was deleted");
                            }
                        };

                        let mut res = arg.persist();
                        let new_script_ctx_ref_resolve = Arc::downgrade(&script_ctx_ref_resolve);
                        let resolver_resolve = Arc::clone(&resolver_resolve);
                        script_ctx_ref_resolve
                            .compiled_library_api
                            .run_on_background(Box::new(move || {
                                let new_script_ctx_ref_resolve = match new_script_ctx_ref_resolve
                                    .upgrade()
                                {
                                    Some(s) => s,
                                    None => {
                                        resolver_resolve.ref_cell.borrow_mut().forget();
                                        res.forget();
                                        log("Library was delete while not all the jobs were done");
                                        return;
                                    }
                                };
                                let isolate_scope = new_script_ctx_ref_resolve.isolate.enter();
                                let ctx_scope =
                                    new_script_ctx_ref_resolve.ctx.enter(&isolate_scope);
                                let _trycatch = isolate_scope.new_try_catch();
                                let res = res.take_local(&isolate_scope);
                                let resolver = resolver_resolve
                                    .ref_cell
                                    .borrow_mut()
                                    .take_local(&isolate_scope)
                                    .as_resolver();
                                resolver.resolve(&ctx_scope, &res);
                            }));
                        Ok(None)
                    }
                ));

                let reject = curr_ctx_scope.new_native_function(new_native_function!(
                    move |_isolate_scope, _curr_ctx_scope, arg: V8LocalValue| {
                        let script_ctx_ref_reject = match script_ctx_ref_reject.upgrade() {
                            Some(s) => s,
                            None => {
                                resolver_reject.ref_cell.borrow_mut().forget();
                                return Err("Library was deleted");
                            }
                        };

                        let mut res = arg.persist();
                        let new_script_ctx_ref_reject = Arc::downgrade(&script_ctx_ref_reject);
                        let resolver_reject = Arc::clone(&resolver_reject);
                        script_ctx_ref_reject
                            .compiled_library_api
                            .run_on_background(Box::new(move || {
                                let new_script_ctx_ref_reject = match new_script_ctx_ref_reject
                                    .upgrade()
                                {
                                    Some(s) => s,
                                    None => {
                                        res.forget();
                                        resolver_reject.ref_cell.borrow_mut().forget();
                                        log("Library was delete while not all the jobs were done");
                                        return;
                                    }
                                };
                                let isolate_scope = new_script_ctx_ref_reject.isolate.enter();
                                let ctx_scope = new_script_ctx_ref_reject.ctx.enter(&isolate_scope);
                                let _trycatch = isolate_scope.new_try_catch();
                                let res = res.take_local(&isolate_scope);
                                let resolver = resolver_reject
                                    .ref_cell
                                    .borrow_mut()
                                    .take_local(&isolate_scope)
                                    .as_resolver();
                                resolver.reject(&ctx_scope, &res);
                            }));
                        Ok(None)
                    }
                ));

                let _ = function.call(
                    curr_ctx_scope,
                    Some(&[&resolve.to_value(), &reject.to_value()]),
                );
                Ok(Some(promise.to_value()))
            }
        ),
    );

    Ok(())
}
