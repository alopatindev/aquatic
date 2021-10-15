use std::sync::atomic::Ordering;
use std::time::Instant;

use histogram::Histogram;

use aquatic_common::AccessListType;

use crate::common::*;
use crate::config::Config;

pub fn clean_connections_and_torrents(config: &Config, state: &State) {
    let now = Instant::now();

    {
        let mut connections = state.connections.lock();

        connections.retain(|_, v| v.0 > now);
        connections.shrink_to_fit();
    }

    match config.access_list.list_type {
        access_list_type @ (AccessListType::Allow | AccessListType::Deny) => {
            let mut access_list = state.access_list.lock();

            if let Err(err) = access_list.update_from_path(&config.access_list.path) {
                ::log::error!("Update access list from path: {:?}", err);
            }

            let mut torrents = state.torrents.lock();

            torrents.ipv4.retain(|info_hash, torrent| {
                access_list.allows(access_list_type, &info_hash.0)
                    && clean_torrent_and_peers(now, torrent)
            });
            torrents.ipv4.shrink_to_fit();

            torrents.ipv6.retain(|info_hash, torrent| {
                access_list.allows(access_list_type, &info_hash.0)
                    && clean_torrent_and_peers(now, torrent)
            });
            torrents.ipv6.shrink_to_fit();
        }
        AccessListType::Ignore => {
            let mut torrents = state.torrents.lock();

            torrents
                .ipv4
                .retain(|_, torrent| clean_torrent_and_peers(now, torrent));
            torrents.ipv4.shrink_to_fit();

            torrents
                .ipv6
                .retain(|_, torrent| clean_torrent_and_peers(now, torrent));
            torrents.ipv6.shrink_to_fit();
        }
    }
}

/// Returns true if torrent is to be kept
#[inline]
fn clean_torrent_and_peers<I: Ip>(now: Instant, torrent: &mut TorrentData<I>) -> bool {
    let num_seeders = &mut torrent.num_seeders;
    let num_leechers = &mut torrent.num_leechers;

    torrent.peers.retain(|_, peer| {
        let keep = peer.valid_until.0 > now;

        if !keep {
            match peer.status {
                PeerStatus::Seeding => {
                    *num_seeders -= 1;
                }
                PeerStatus::Leeching => {
                    *num_leechers -= 1;
                }
                _ => (),
            };
        }

        keep
    });

    !torrent.peers.is_empty()
}

pub fn gather_and_print_statistics(state: &State, config: &Config) {
    let interval = config.statistics.interval;

    let requests_received: f64 = state
        .statistics
        .requests_received
        .fetch_and(0, Ordering::SeqCst) as f64;
    let responses_sent: f64 = state
        .statistics
        .responses_sent
        .fetch_and(0, Ordering::SeqCst) as f64;
    let bytes_received: f64 = state
        .statistics
        .bytes_received
        .fetch_and(0, Ordering::SeqCst) as f64;
    let bytes_sent: f64 = state.statistics.bytes_sent.fetch_and(0, Ordering::SeqCst) as f64;

    let requests_per_second = requests_received / interval as f64;
    let responses_per_second: f64 = responses_sent / interval as f64;
    let bytes_received_per_second: f64 = bytes_received / interval as f64;
    let bytes_sent_per_second: f64 = bytes_sent / interval as f64;

    let readable_events: f64 = state
        .statistics
        .readable_events
        .fetch_and(0, Ordering::SeqCst) as f64;
    let requests_per_readable_event = if readable_events == 0.0 {
        0.0
    } else {
        requests_received / readable_events
    };

    println!(
        "stats: {:.2} requests/second, {:.2} responses/second, {:.2} requests/readable event",
        requests_per_second, responses_per_second, requests_per_readable_event
    );

    println!(
        "bandwidth: {:7.2} Mbit/s in, {:7.2} Mbit/s out",
        bytes_received_per_second * 8.0 / 1_000_000.0,
        bytes_sent_per_second * 8.0 / 1_000_000.0,
    );

    let mut peers_per_torrent = Histogram::new();

    {
        let torrents = &mut state.torrents.lock();

        for torrent in torrents.ipv4.values() {
            let num_peers = (torrent.num_seeders + torrent.num_leechers) as u64;

            if let Err(err) = peers_per_torrent.increment(num_peers) {
                ::log::error!("error incrementing peers_per_torrent histogram: {}", err)
            }
        }
        for torrent in torrents.ipv6.values() {
            let num_peers = (torrent.num_seeders + torrent.num_leechers) as u64;

            if let Err(err) = peers_per_torrent.increment(num_peers) {
                ::log::error!("error incrementing peers_per_torrent histogram: {}", err)
            }
        }
    }

    if peers_per_torrent.entries() != 0 {
        println!(
            "peers per torrent: min: {}, p50: {}, p75: {}, p90: {}, p99: {}, p999: {}, max: {}",
            peers_per_torrent.minimum().unwrap(),
            peers_per_torrent.percentile(50.0).unwrap(),
            peers_per_torrent.percentile(75.0).unwrap(),
            peers_per_torrent.percentile(90.0).unwrap(),
            peers_per_torrent.percentile(99.0).unwrap(),
            peers_per_torrent.percentile(99.9).unwrap(),
            peers_per_torrent.maximum().unwrap(),
        );
    }

    println!();
}
