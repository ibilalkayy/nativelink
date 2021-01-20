// Copyright 2020-2021 Nathan (Blaise) Bruer.  All rights reserved.

use std::convert::TryFrom;
use std::io::Cursor;
use std::pin::Pin;

use bytestream_server::ByteStreamServer;
use maplit::hashmap;
use tonic::Request;

use common::DigestInfo;
use config;
use error::Error;
use store::StoreManager;

const INSTANCE_NAME: &str = "foo_instance_name";
const HASH1: &str = "0123456789abcdef000000000000000000000000000000000123456789abcdef";

fn make_store_manager() -> Result<StoreManager, Error> {
    let mut store_manager = StoreManager::new();
    store_manager.make_store(
        "main_cas",
        &config::backends::StoreConfig::memory(config::backends::MemoryStore::default()),
    )?;
    Ok(store_manager)
}

fn make_bytestream_server(store_manager: &mut StoreManager) -> Result<ByteStreamServer, Error> {
    ByteStreamServer::new(
        &config::cas_server::ByteStreamConfig {
            cas_stores: hashmap! {
                "foo_instance_name".to_string() => "main_cas".to_string(),
            },
            max_bytes_per_stream: 1024,
            read_buffer_stream_size: 1024,
            write_buffer_stream_size: 1024,
        },
        &store_manager,
    )
}

#[cfg(test)]
pub mod write_tests {
    use super::*;
    use pretty_assertions::assert_eq; // Must be declared in every module.

    use prost::{bytes::Bytes, Message};
    use tonic::{
        codec::Codec, // Needed for .decoder().
        codec::ProstCodec,
        transport::Body,
        Streaming,
    };

    use proto::google::bytestream::{
        byte_stream_server::ByteStream, // Needed to call .write().
        WriteRequest,
    };

    // Utility to encode our proto into GRPC stream format.
    fn encode<T: Message>(proto: &T) -> Result<Bytes, Box<dyn std::error::Error>> {
        use bytes::{BufMut, BytesMut};
        let mut buf = BytesMut::new();
        // See below comment on spec.
        use std::mem::size_of;
        const PREFIX_BYTES: usize = size_of::<u8>() + size_of::<u32>();
        for _ in 0..PREFIX_BYTES {
            // Advance our buffer first.
            // We will backfill it once we know the size of the message.
            buf.put_u8(0);
        }
        proto.encode(&mut buf)?;
        let len = buf.len() - PREFIX_BYTES;
        {
            let mut buf = &mut buf[0..PREFIX_BYTES];
            // See: https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md#:~:text=Compressed-Flag
            // for more details on spec.
            // Compressed-Flag -> 0 / 1 # encoded as 1 byte unsigned integer.
            buf.put_u8(0);
            // Message-Length -> {length of Message} # encoded as 4 byte unsigned integer (big endian).
            buf.put_u32(len as u32);
            // Message -> *{binary octet}.
        }

        Ok(buf.freeze())
    }

    #[tokio::test]
    pub async fn chunked_stream_receives_all_data() -> Result<(), Box<dyn std::error::Error>> {
        let mut store_manager = make_store_manager()?;
        let bs_server = make_bytestream_server(&mut store_manager)?;
        let store_owned = store_manager.get_store("main_cas").unwrap();

        let store = Pin::new(store_owned.as_ref());

        // Setup stream.
        let (mut tx, join_handle) = {
            let (tx, body) = Body::channel();
            let mut codec = ProstCodec::<WriteRequest, WriteRequest>::default();
            // Note: This is an undocumented function.
            let stream = Streaming::new_request(codec.decoder(), body);

            let join_handle = tokio::spawn(async move {
                let response_future = bs_server.write(Request::new(stream));
                response_future.await
            });
            (tx, join_handle)
        };
        // Send data.
        let raw_data = {
            let raw_data = "12456789abcdefghijk".as_bytes();
            // Chunk our data into two chunks to simulate something a client
            // might do.
            const BYTE_SPLIT_OFFSET: usize = 8;

            let resource_name = format!(
                "{}/uploads/{}/blobs/{}/{}",
                INSTANCE_NAME,
                "4dcec57e-1389-4ab5-b188-4a59f22ceb4b", // Randomly generated.
                HASH1,
                raw_data.len()
            );
            let mut write_request = WriteRequest {
                resource_name: resource_name,
                write_offset: 0,
                finish_write: false,
                data: vec![],
            };
            // Write first chunk of data.
            write_request.write_offset = 0;
            write_request.data = raw_data[..BYTE_SPLIT_OFFSET].to_vec();
            tx.send_data(encode(&write_request)?).await?;

            // Write empty set of data (clients are allowed to do this.
            write_request.write_offset = BYTE_SPLIT_OFFSET as i64;
            write_request.data = vec![];
            tx.send_data(encode(&write_request)?).await?;

            // Write final bit of data.
            write_request.write_offset = BYTE_SPLIT_OFFSET as i64;
            write_request.data = raw_data[BYTE_SPLIT_OFFSET..].to_vec();
            write_request.finish_write = true;
            tx.send_data(encode(&write_request)?).await?;

            raw_data
        };
        // Check results of server.
        {
            // One for spawn() future and one for result.
            let server_result = join_handle.await??;
            let committed_size =
                usize::try_from(server_result.into_inner().committed_size).or(Err("Cant convert i64 to usize"))?;
            assert_eq!(committed_size as usize, raw_data.len());

            // Now lets check our store to ensure it was written with proper data.
            store.has(DigestInfo::try_new(&HASH1, raw_data.len())?).await?;
            let mut store_data = Vec::new();
            store
                .get(
                    DigestInfo::try_new(&HASH1, raw_data.len())?,
                    &mut Cursor::new(&mut store_data),
                )
                .await?;
            assert_eq!(
                std::str::from_utf8(&store_data),
                std::str::from_utf8(&raw_data),
                "Expected store to have been updated to new value"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
pub mod read_tests {
    use super::*;
    use pretty_assertions::assert_eq; // Must be declared in every module.

    use tokio::stream::StreamExt;

    use proto::google::bytestream::{
        byte_stream_server::ByteStream, // Needed to call .read().
        ReadRequest,
    };

    #[tokio::test]
    pub async fn chunked_stream_reads_small_set_of_data() -> Result<(), Box<dyn std::error::Error>> {
        let mut store_manager = make_store_manager()?;
        let bs_server = make_bytestream_server(&mut store_manager)?;
        let store_owned = store_manager.get_store("main_cas").unwrap();

        let store = Pin::new(store_owned.as_ref());

        const VALUE1: &str = "12456789abcdefghijk";

        let digest = DigestInfo::try_new(&HASH1, VALUE1.len())?;
        store.update(digest, Box::new(Cursor::new(VALUE1))).await?;

        let read_request = ReadRequest {
            resource_name: format!(
                "{}/uploads/{}/blobs/{}/{}",
                INSTANCE_NAME,
                "4dcec57e-1389-4ab5-b188-4a59f22ceb4b", // Randomly generated.
                HASH1,
                VALUE1.len()
            ),
            read_offset: 0,
            read_limit: VALUE1.len() as i64,
        };
        let mut read_stream = bs_server.read(Request::new(read_request)).await?.into_inner();
        {
            let mut roundtrip_data = Vec::with_capacity(VALUE1.len());
            assert!(VALUE1.len() > 0, "Expected at least one byte to be sent");
            while let Some(result_read_response) = read_stream.next().await {
                roundtrip_data.append(&mut result_read_response?.data);
            }
            assert_eq!(
                roundtrip_data,
                VALUE1.as_bytes(),
                "Expected response to match what is in store"
            );
        }
        Ok(())
    }

    #[tokio::test]
    pub async fn chunked_stream_reads_10mb_of_data() -> Result<(), Box<dyn std::error::Error>> {
        let mut store_manager = make_store_manager()?;
        let bs_server = make_bytestream_server(&mut store_manager)?;
        let store_owned = store_manager.get_store("main_cas").unwrap();

        let store = Pin::new(store_owned.as_ref());

        const DATA_SIZE: usize = 10_000_000;
        let mut raw_data = vec![41u8; DATA_SIZE];
        // Change just a few bits to ensure we don't get same packet
        // over and over.
        raw_data[5] = 42u8;
        raw_data[DATA_SIZE - 2] = 43u8;

        let digest = DigestInfo::try_new(&HASH1, raw_data.len())?;
        store.update(digest, Box::new(Cursor::new(raw_data.as_slice()))).await?;

        let read_request = ReadRequest {
            resource_name: format!(
                "{}/uploads/{}/blobs/{}/{}",
                INSTANCE_NAME,
                "4dcec57e-1389-4ab5-b188-4a59f22ceb4b", // Randomly generated.
                HASH1,
                raw_data.len()
            ),
            read_offset: 0,
            read_limit: raw_data.len() as i64,
        };
        let mut read_stream = bs_server.read(Request::new(read_request)).await?.into_inner();
        {
            let mut roundtrip_data = Vec::with_capacity(raw_data.len());
            assert!(raw_data.len() > 0, "Expected at least one byte to be sent");
            while let Some(result_read_response) = read_stream.next().await {
                roundtrip_data.append(&mut result_read_response?.data);
            }
            assert_eq!(roundtrip_data, raw_data, "Expected response to match what is in store");
        }
        Ok(())
    }
}