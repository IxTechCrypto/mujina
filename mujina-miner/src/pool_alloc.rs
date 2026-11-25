//! Weighted time-slicing for a single hash chain.
//!
//! The Nano 3S has one chain, so pool weights are a share of hash *time*,
//! not a split of chips. One slot (60 s) belongs to one pool. Over many
//! slots the number of slots each pool receives matches its weight.
//!
//! A pool with weight 0 is not in the rotation. It is used only when every
//! positive-weight pool is down, in list order. A down pool's time is given
//! to the live positive-weight pools in proportion to their weights.

/// One rotation slot. The scheduler switches jobs at most once per slot.
pub const SLOT: std::time::Duration = std::time::Duration::from_secs(60);

/// A pool as the rotator sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolView {
    pub weight: u16,
    pub up: bool,
}

/// Share of hash time, in percent, after failover.
///
/// Positive-weight pools that are up keep their relative weights, scaled so
/// the live ones sum to 100. If none of those are up, the first pool that
/// is up (including a weight-0 backup) gets 100. If nothing is up, every
/// share is 0.
pub fn time_shares(pools: &[PoolView]) -> Vec<u16> {
    let mut out = vec![0u16; pools.len()];
    let live: Vec<usize> = pools
        .iter()
        .enumerate()
        .filter(|(_, p)| p.up && p.weight > 0)
        .map(|(i, _)| i)
        .collect();
    if !live.is_empty() {
        let sum: u32 = live.iter().map(|&i| u32::from(pools[i].weight)).sum();
        let mut assigned = 0u16;
        for (n, &i) in live.iter().enumerate() {
            let share = if n + 1 == live.len() {
                100 - assigned
            } else {
                ((u32::from(pools[i].weight) * 100) / sum) as u16
            };
            out[i] = share;
            assigned = assigned.saturating_add(share);
        }
        return out;
    }
    if let Some(i) = pools.iter().position(|p| p.up) {
        out[i] = 100;
    }
    out
}

/// Picks one pool per slot. `credit[i]` is that pool's accumulated claim.
///
/// Each call adds the pool's current time-share, then gives the slot to the
/// largest credit and subtracts 100 from it. Pools with a zero share are
/// not candidates. Credits stay bounded, so a long run matches the weights.
pub fn pick_slot(credit: &mut [i64], shares: &[u16]) -> Option<usize> {
    let mut best: Option<(usize, i64)> = None;
    for (i, &share) in shares.iter().enumerate() {
        if share == 0 {
            continue;
        }
        let c = credit.get_mut(i)?;
        *c += i64::from(share);
        let now = *c;
        if best.map(|(_, b)| now > b).unwrap_or(true) {
            best = Some((i, now));
        }
    }
    let (i, _) = best?;
    if let Some(c) = credit.get_mut(i) {
        *c -= 100;
    }
    Some(i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn views(weights: &[u16], up: &[bool]) -> Vec<PoolView> {
        weights
            .iter()
            .zip(up)
            .map(|(&weight, &up)| PoolView { weight, up })
            .collect()
    }

    #[test]
    fn one_pool_at_100_percent_gets_every_slot() {
        let shares = time_shares(&views(&[100], &[true]));
        assert_eq!(shares, vec![100]);
        let mut credit = vec![0i64; 1];
        for _ in 0..5 {
            assert_eq!(pick_slot(&mut credit, &shares), Some(0));
        }
    }

    #[test]
    fn rotation_matches_weights_over_ten_slots() {
        let shares = time_shares(&views(&[50, 30, 20], &[true, true, true]));
        assert_eq!(shares, vec![50, 30, 20]);
        let mut credit = vec![0i64; 3];
        let mut counts = [0u32; 3];
        for _ in 0..10 {
            let i = pick_slot(&mut credit, &shares).unwrap();
            counts[i] += 1;
        }
        assert_eq!(counts, [5, 3, 2]);
    }

    #[test]
    fn a_down_pool_gives_its_time_to_the_others_in_proportion() {
        let shares = time_shares(&views(&[50, 30, 20], &[true, false, true]));
        assert_eq!(shares.iter().sum::<u16>(), 100);
        assert_eq!(shares[1], 0);
        // 50:20 of the remaining time is 71 and 29, with the last pool
        // taking the rounding remainder.
        assert_eq!(shares[0], 71);
        assert_eq!(shares[2], 29);
    }

    #[test]
    fn weight_zero_is_only_used_when_every_weighted_pool_is_down() {
        let normal = time_shares(&views(&[100, 0], &[true, true]));
        assert_eq!(normal, vec![100, 0]);
        let failed = time_shares(&views(&[70, 30, 0], &[false, false, true]));
        assert_eq!(failed, vec![0, 0, 100]);
        let none = time_shares(&views(&[100, 0], &[false, false]));
        assert_eq!(none, vec![0, 0]);
    }

    #[test]
    fn failover_follows_list_order() {
        let shares = time_shares(&views(&[0, 0, 100], &[true, true, false]));
        assert_eq!(shares, vec![100, 0, 0]);
    }
}
