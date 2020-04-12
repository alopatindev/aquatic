use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use std::vec::Drain;

use parking_lot::MutexGuard;
use crossbeam_channel::{Sender, Receiver};
use rand::{SeedableRng, Rng, rngs::{SmallRng, StdRng}};

use bittorrent_udp::types::*;

use crate::common::*;
use crate::config::Config;


pub fn run_request_worker(
    state: State,
    config: Config,
    request_receiver: Receiver<(Request, SocketAddr)>,
    response_sender: Sender<(Response, SocketAddr)>,
){
    let mut connect_requests: Vec<(ConnectRequest, SocketAddr)> = Vec::new();
    let mut announce_requests: Vec<(AnnounceRequest, SocketAddr)> = Vec::new();
    let mut scrape_requests: Vec<(ScrapeRequest, SocketAddr)> = Vec::new();

    let mut responses: Vec<(Response, SocketAddr)> = Vec::new();

    let mut std_rng = StdRng::from_entropy();
    let mut small_rng = SmallRng::from_rng(&mut std_rng).unwrap();

    let timeout = Duration::from_millis(
        config.handlers.channel_recv_timeout_ms
    );

    loop {
        let mut opt_data = None;

        // Collect requests from channel, divide them by type
        //
        // Collect a maximum number of request. Stop collecting before that
        // number is reached if having waited for too long for a request, but
        // only if HandlerData mutex isn't locked.
        for i in 0..config.handlers.max_requests_per_iter {
            let (request, src): (Request, SocketAddr) = if i == 0 {
                match request_receiver.recv(){
                    Ok(r) => r,
                    Err(_) => break, // Really shouldn't happen
                }
            } else {
                match request_receiver.recv_timeout(timeout){
                    Ok(r) => r,
                    Err(_) => {
                        if let Some(data) = state.handler_data.try_lock(){
                            opt_data = Some(data);

                            break
                        } else {
                            continue
                        }
                    },
                }
            };

            match request {
                Request::Connect(r) => {
                    connect_requests.push((r, src))
                },
                Request::Announce(r) => {
                    announce_requests.push((r, src))
                },
                Request::Scrape(r) => {
                    scrape_requests.push((r, src))
                },
            }
        }

        let mut data: MutexGuard<HandlerData> = opt_data.unwrap_or_else(||
            state.handler_data.lock()
        );

        handle_connect_requests(
            &mut data,
            &mut std_rng,
            connect_requests.drain(..),
            &mut responses
        );

        handle_announce_requests(
            &mut data,
            &config,
            &mut small_rng,
            announce_requests.drain(..),
            &mut responses
        );
        handle_scrape_requests(
            &mut data,
            scrape_requests.drain(..),
            &mut responses
        );

        ::std::mem::drop(data);

        for r in responses.drain(..){
            if let Err(err) = response_sender.send(r){
                eprintln!("error sending response to channel: {}", err);
            }
        }
    }
}


#[inline]
pub fn handle_connect_requests(
    data: &mut MutexGuard<HandlerData>,
    rng: &mut StdRng,
    requests: Drain<(ConnectRequest, SocketAddr)>,
    responses: &mut Vec<(Response, SocketAddr)>,
){
    let now = Time(Instant::now());

    responses.extend(requests.map(|(request, src)| {
        let connection_id = ConnectionId(rng.gen());

        let key = ConnectionKey {
            connection_id,
            socket_addr: src,
        };

        data.connections.insert(key, now);

        let response = Response::Connect(
            ConnectResponse {
                connection_id,
                transaction_id: request.transaction_id,
            }
        );
        
        (response, src)
    }));
}


#[inline]
pub fn handle_announce_requests(
    data: &mut MutexGuard<HandlerData>,
    config: &Config,
    rng: &mut SmallRng,
    requests: Drain<(AnnounceRequest, SocketAddr)>,
    responses: &mut Vec<(Response, SocketAddr)>,
){
    responses.extend(requests.map(|(request, src)| {
        let connection_key = ConnectionKey {
            connection_id: request.connection_id,
            socket_addr: src,
        };

        if !data.connections.contains_key(&connection_key){
            let response = ErrorResponse {
                transaction_id: request.transaction_id,
                message: "Connection invalid or expired".to_string()
            };

            return (response.into(), src);
        }

        let peer_key = PeerMapKey {
            ip: src.ip(),
            peer_id: request.peer_id,
        };

        let peer = Peer::from_announce_and_ip(&request, src.ip());
        let peer_status = peer.status;

        let torrent_data = data.torrents
            .entry(request.info_hash)
            .or_default();
        
        let opt_removed_peer_status = if peer_status == PeerStatus::Stopped {
            torrent_data.peers.remove(&peer_key)
                .map(|peer| peer.status)
        } else {
            torrent_data.peers.insert(peer_key, peer)
                .map(|peer| peer.status)
        };

        let max_num_peers_to_take = (request.peers_wanted.0.max(0) as usize)
            .min(config.network.max_response_peers);

        match peer_status {
            PeerStatus::Leeching => {
                torrent_data.num_leechers.fetch_add(1, Ordering::SeqCst);
            },
            PeerStatus::Seeding => {
                torrent_data.num_seeders.fetch_add(1, Ordering::SeqCst);
            },
            PeerStatus::Stopped => {}
        };

        match opt_removed_peer_status {
            Some(PeerStatus::Leeching) => {
                torrent_data.num_leechers.fetch_sub(1, Ordering::SeqCst);
            },
            Some(PeerStatus::Seeding) => {
                torrent_data.num_seeders.fetch_sub(1, Ordering::SeqCst);
            },
            _ => {}
        }

        let response_peers = extract_response_peers(
            rng,
            &torrent_data.peers,
            max_num_peers_to_take,
        );

        let response = Response::Announce(AnnounceResponse {
            transaction_id: request.transaction_id,
            announce_interval: AnnounceInterval(config.network.peer_announce_interval),
            leechers: NumberOfPeers(torrent_data.num_leechers.load(Ordering::SeqCst) as i32),
            seeders: NumberOfPeers(torrent_data.num_seeders.load(Ordering::SeqCst) as i32),
            peers: response_peers
        });

        (response, src)
    }));
}


#[inline]
pub fn handle_scrape_requests(
    data: &mut MutexGuard<HandlerData>,
    requests: Drain<(ScrapeRequest, SocketAddr)>,
    responses: &mut Vec<(Response, SocketAddr)>,
){
    let empty_stats = create_torrent_scrape_statistics(0, 0);

    responses.extend(requests.map(|(request, src)|{
        let connection_key = ConnectionKey {
            connection_id: request.connection_id,
            socket_addr: src,
        };

        if !data.connections.contains_key(&connection_key){
            let response = ErrorResponse {
                transaction_id: request.transaction_id,
                message: "Connection invalid or expired".to_string()
            };

            return (response.into(), src);
        }

        let mut stats: Vec<TorrentScrapeStatistics> = Vec::with_capacity(
            request.info_hashes.len()
        );

        for info_hash in request.info_hashes.iter() {
            if let Some(torrent_data) = data.torrents.get(info_hash){
                stats.push(create_torrent_scrape_statistics(
                    torrent_data.num_seeders.load(Ordering::SeqCst) as i32,
                    torrent_data.num_leechers.load(Ordering::SeqCst) as i32,
                ));
            } else {
                stats.push(empty_stats);
            }
        }

        let response = Response::Scrape(ScrapeResponse {
            transaction_id: request.transaction_id,
            torrent_stats: stats,
        });

        (response, src)
    }));
}


/// Extract response peers
/// 
/// If there are more peers in map that `number_of_peers_to_take`, do a
/// half-random selection of peers from first and second halves of map,
/// in order to avoid returning too homogeneous peers.
/// 
/// Don't care if we send back announcing peer.
#[inline]
pub fn extract_response_peers(
    rng: &mut impl Rng,
    peer_map: &PeerMap,
    max_num_peers_to_take: usize,
) -> Vec<ResponsePeer> {
    let peer_map_len = peer_map.len();

    if peer_map_len <= max_num_peers_to_take {
        peer_map.values()
            .map(Peer::to_response_peer)
            .collect()
    } else {
        let half_num_to_take = max_num_peers_to_take / 2;
        let half_peer_map_len = peer_map_len / 2;

        let offset_first_half = rng.gen_range(
            0,
            (half_peer_map_len + (peer_map_len % 2)) - half_num_to_take
        );
        let offset_second_half = rng.gen_range(
            half_peer_map_len,
            peer_map_len - half_num_to_take
        );

        let end_first_half = offset_first_half + half_num_to_take;
        let end_second_half = offset_second_half + half_num_to_take + (max_num_peers_to_take % 2);

        let mut peers: Vec<ResponsePeer> = Vec::with_capacity(max_num_peers_to_take);

        for i in offset_first_half..end_first_half {
            if let Some((_, peer)) = peer_map.get_index(i){
                peers.push(peer.to_response_peer())
            }
        }
        for i in offset_second_half..end_second_half {
            if let Some((_, peer)) = peer_map.get_index(i){
                peers.push(peer.to_response_peer())
            }
        }
        
        debug_assert_eq!(peers.len(), max_num_peers_to_take);

        peers
    }
}


#[inline(always)]
pub fn create_torrent_scrape_statistics(
    seeders: i32,
    leechers: i32
) -> TorrentScrapeStatistics {
    TorrentScrapeStatistics {
        seeders: NumberOfPeers(seeders),
        completed: NumberOfDownloads(0), // No implementation planned
        leechers: NumberOfPeers(leechers)
    }
}


#[cfg(test)]
mod tests {
    use std::time::Instant;
    use std::net::IpAddr;
    use std::collections::HashSet;

    use indexmap::IndexMap;
    use rand::thread_rng;
    use quickcheck::{TestResult, quickcheck};

    use super::*;

    fn gen_peer_map_key_and_value(i: u32) -> (PeerMapKey, Peer) {
        let ip_address = IpAddr::from(i.to_be_bytes());
        let peer_id = PeerId([0; 20]);

        let key = PeerMapKey {
            ip: ip_address, 
            peer_id,
        };
        let value = Peer {
            ip_address,
            port: Port(1),
            status: PeerStatus::Leeching,
            last_announce: Time(Instant::now()),
        };

        (key, value)
    }

    #[test]
    fn test_extract_response_peers(){
        fn prop(data: (u32, u16)) -> TestResult {
            let gen_num_peers = data.0;
            let req_num_peers = data.1 as usize;

            let mut peer_map: PeerMap = IndexMap::new();

            for i in 0..gen_num_peers {
                let (key, value) = gen_peer_map_key_and_value(i);

                peer_map.insert(key, value);
            }

            let mut rng = thread_rng();

            let peers = extract_response_peers(
                &mut rng,
                &peer_map,
                req_num_peers
            );

            // Check that number of returned peers is correct

            let mut success = peers.len() <= req_num_peers;

            if req_num_peers >= gen_num_peers as usize {
                success &= peers.len() == gen_num_peers as usize;
            }

            // Check that returned peers are unique (no overlap)

            let mut ip_addresses = HashSet::new();

            for peer in peers {
                if ip_addresses.contains(&peer.ip_address){
                    success = false;

                    break;
                }

                ip_addresses.insert(peer.ip_address);
            }

            TestResult::from_bool(success)
        }   

        quickcheck(prop as fn((u32, u16)) -> TestResult);
    }
}