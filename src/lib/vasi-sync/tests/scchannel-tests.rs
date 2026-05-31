//! This file contains tests intended to be run using [loom]. See the
//! [crate-level documentation](crate) for details about running these under
//! loom.
//!
//! [loom]: <https://docs.rs/loom/latest/loom/>

mod sync;

mod scchannel_tests {
    use vasi_sync::scchannel::{SelfContainedChannel, SelfContainedChannelError};

    use super::*;

    #[test]
    fn test_single_thread() {
        sync::model(|| {
            let channel = SelfContainedChannel::new();
            channel.send(42);
            let val = channel.receive().unwrap();
            assert_eq!(val, 42);
        })
    }

    #[test]
    fn test_two_threads() {
        sync::model(|| {
            let channel = sync::Arc::new(SelfContainedChannel::new());
            let writer = {
                let channel = channel.clone();
                sync::thread::spawn(move || {
                    channel.send(42);
                })
            };
            let reader = sync::thread::spawn(move || channel.receive());
            writer.join().unwrap();
            assert_eq!(reader.join().unwrap(), Ok(42));
        })
    }

    #[test]
    fn test_drop_cross_thread() {
        sync::model(|| {
            let channel = SelfContainedChannel::new();
            let writer = {
                sync::thread::spawn(move || {
                    channel.send(Box::new(42));
                    channel
                })
            };
            let channel = writer.join().unwrap();
            // Since this occurs after joining the writer thread, the contents
            // will be dropped from this thread.
            drop(channel);
        })
    }

    #[test]
    fn test_writer_writes_then_closes() {
        sync::model(|| {
            let channel = sync::Arc::new(SelfContainedChannel::<u32>::new());
            let writer = {
                let channel = channel.clone();
                sync::thread::spawn(move || {
                    channel.send(42);
                    channel.close_writer();
                })
            };
            let reader = {
                let channel = channel.clone();
                sync::thread::spawn(move || channel.receive())
            };
            writer.join().unwrap();
            // Reader should have gotten the written value.
            assert_eq!(reader.join().unwrap(), Ok(42));

            // Reading from the channel again should return an error since
            // the writer has closed. (And shouldn't deadlock or panic).
            assert_eq!(
                channel.receive(),
                Err(SelfContainedChannelError::WriterIsClosed)
            );
        })
    }

    #[test]
    fn test_writer_close_watchdog_with_write() {
        sync::model_with_max_preemptions(3, || {
            let channel = sync::Arc::new(SelfContainedChannel::<u32>::new());
            let writer = {
                let channel = channel.clone();
                sync::thread::spawn(move || channel.send(42))
            };
            let reader = {
                let channel = channel.clone();
                sync::thread::spawn(move || channel.receive())
            };
            // Simulate a separate watchdog thread that detects that the writer process
            // has exited, and closes the channel in parallel with the other operations.
            let watchdog = sync::thread::spawn(move || channel.close_writer());
            let res = reader.join().unwrap();
            // We should either get the written value, or an error, depending on
            // the execution order.
            assert!(res == Ok(42) || res == Err(SelfContainedChannelError::WriterIsClosed));
            writer.join().unwrap();
            watchdog.join().unwrap();
        })
    }

    #[test]
    fn test_writer_close_watchdog_without_write() {
        sync::model(|| {
            let channel = sync::Arc::new(SelfContainedChannel::<u32>::new());
            let reader = {
                let channel = channel.clone();
                sync::thread::spawn(move || channel.receive())
            };
            // Parallel with channel.receive()
            channel.close_writer();
            let res = reader.join().unwrap();
            // We should either get the written value, or an error, depending on
            // the execution order.
            assert_eq!(res, Err(SelfContainedChannelError::WriterIsClosed));
        })
    }

    #[test]
    fn test_channel_reuse() {
        // Test reusing channels, using another channel to synchronize.
        // This is analagous to how shadow communicates with a plugin.
        sync::model_with_max_preemptions(3, || {
            let alpha_to_beta = sync::Arc::new(SelfContainedChannel::<u32>::new());
            let beta_to_alpha = sync::Arc::new(SelfContainedChannel::<u32>::new());

            let alpha = {
                let send_channel = alpha_to_beta.clone();
                let recv_channel = beta_to_alpha.clone();
                sync::thread::spawn(move || {
                    let mut v = Vec::new();
                    send_channel.send(1);
                    v.push(recv_channel.receive());
                    send_channel.send(2);
                    v.push(recv_channel.receive());
                    send_channel.send(3);
                    v.push(recv_channel.receive());
                    v
                })
            };
            let beta = {
                let send_channel = beta_to_alpha;
                let recv_channel = alpha_to_beta;
                sync::thread::spawn(move || {
                    let mut v = Vec::new();
                    v.push(recv_channel.receive());
                    send_channel.send(4);
                    v.push(recv_channel.receive());
                    send_channel.send(5);
                    v.push(recv_channel.receive());
                    send_channel.send(6);
                    v
                })
            };
            assert_eq!(alpha.join().unwrap(), vec![Ok(4), Ok(5), Ok(6)]);
            assert_eq!(beta.join().unwrap(), vec![Ok(1), Ok(2), Ok(3)]);
        })
    }

    #[test]
    fn test_channel_reuse_fast_receive() {
        // Same as `test_channel_reuse`, but using the fast SPSC receive path
        // used by Shadow/shim IPC.
        sync::model_with_max_preemptions(3, || {
            let alpha_to_beta = sync::Arc::new(SelfContainedChannel::<u32>::new());
            let beta_to_alpha = sync::Arc::new(SelfContainedChannel::<u32>::new());

            let alpha = {
                let send_channel = alpha_to_beta.clone();
                let recv_channel = beta_to_alpha.clone();
                sync::thread::spawn(move || {
                    let mut v = Vec::new();
                    send_channel.send(1);
                    // SAFETY: This test has one receiver for this channel.
                    v.push(unsafe { recv_channel.receive_assuming_single_consumer() });
                    send_channel.send(2);
                    // SAFETY: This test has one receiver for this channel.
                    v.push(unsafe { recv_channel.receive_assuming_single_consumer() });
                    send_channel.send(3);
                    // SAFETY: This test has one receiver for this channel.
                    v.push(unsafe { recv_channel.receive_assuming_single_consumer() });
                    v
                })
            };
            let beta = {
                let send_channel = beta_to_alpha;
                let recv_channel = alpha_to_beta;
                sync::thread::spawn(move || {
                    let mut v = Vec::new();
                    // SAFETY: This test has one receiver for this channel.
                    v.push(unsafe { recv_channel.receive_assuming_single_consumer() });
                    send_channel.send(4);
                    // SAFETY: This test has one receiver for this channel.
                    v.push(unsafe { recv_channel.receive_assuming_single_consumer() });
                    send_channel.send(5);
                    // SAFETY: This test has one receiver for this channel.
                    v.push(unsafe { recv_channel.receive_assuming_single_consumer() });
                    send_channel.send(6);
                    v
                })
            };
            assert_eq!(alpha.join().unwrap(), vec![Ok(4), Ok(5), Ok(6)]);
            assert_eq!(beta.join().unwrap(), vec![Ok(1), Ok(2), Ok(3)]);
        })
    }

    #[test]
    fn test_try_receive_single_thread() {
        sync::model(|| {
            let channel = SelfContainedChannel::new();
            assert!(channel.is_empty());

            // SAFETY: This test has one receiver for this channel.
            assert_eq!(
                unsafe { channel.try_receive_assuming_single_consumer() },
                Ok(None)
            );
            assert!(channel.is_empty());

            channel.send(42);
            assert!(!channel.is_empty());
            // SAFETY: This test has one receiver for this channel.
            assert_eq!(
                unsafe { channel.try_receive_assuming_single_consumer() },
                Ok(Some(42))
            );
            assert!(channel.is_empty());

            // SAFETY: This test has one receiver for this channel.
            assert_eq!(
                unsafe { channel.try_receive_assuming_single_consumer() },
                Ok(None)
            );
            assert!(channel.is_empty());

            channel.close_writer();
            assert!(channel.is_empty());
            // SAFETY: This test has one receiver for this channel.
            assert_eq!(
                unsafe { channel.try_receive_assuming_single_consumer() },
                Err(SelfContainedChannelError::WriterIsClosed)
            );
        })
    }

    #[test]
    fn test_try_receive_gets_pending_message_after_close() {
        sync::model(|| {
            let channel = SelfContainedChannel::new();
            channel.send(42);
            channel.close_writer();

            // SAFETY: This test has one receiver for this channel.
            assert_eq!(
                unsafe { channel.try_receive_assuming_single_consumer() },
                Ok(Some(42))
            );

            // SAFETY: This test has one receiver for this channel.
            assert_eq!(
                unsafe { channel.try_receive_assuming_single_consumer() },
                Err(SelfContainedChannelError::WriterIsClosed)
            );
        })
    }

    #[test]
    fn test_wait_ready_does_not_consume() {
        sync::model(|| {
            let channel = SelfContainedChannel::new();
            channel.send(42);

            // SAFETY: This test has one receiver/waiter for this channel.
            assert_eq!(unsafe { channel.wait_ready_assuming_single_consumer() }, Ok(()));
            assert!(!channel.is_empty());

            // SAFETY: This test has one receiver/waiter for this channel.
            assert_eq!(
                unsafe { channel.try_receive_assuming_single_consumer() },
                Ok(Some(42))
            );
            assert!(channel.is_empty());
        })
    }

    #[test]
    fn test_wait_ready_cross_thread() {
        sync::model(|| {
            let channel = sync::Arc::new(SelfContainedChannel::new());
            let writer = {
                let channel = channel.clone();
                sync::thread::spawn(move || {
                    channel.send(42);
                })
            };
            let waiter = {
                let channel = channel.clone();
                sync::thread::spawn(move || {
                    // SAFETY: This test has one receiver/waiter for this channel.
                    unsafe { channel.wait_ready_assuming_single_consumer() }
                })
            };
            writer.join().unwrap();
            assert_eq!(waiter.join().unwrap(), Ok(()));

            // SAFETY: This test has one receiver/waiter for this channel.
            assert_eq!(
                unsafe { channel.try_receive_assuming_single_consumer() },
                Ok(Some(42))
            );
        })
    }

    #[test]
    fn test_wait_ready_closed_without_message() {
        sync::model(|| {
            let channel = SelfContainedChannel::<u32>::new();
            channel.close_writer();

            // SAFETY: This test has one receiver/waiter for this channel.
            assert_eq!(
                unsafe { channel.wait_ready_assuming_single_consumer() },
                Err(SelfContainedChannelError::WriterIsClosed)
            );
        })
    }
}
