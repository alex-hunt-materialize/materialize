// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::{
    collections::{BTreeMap, btree_map::Entry},
    iter,
};

use mz_ore::now::to_datetime;
use mz_repr::{Datum, Diff, Row};
use mz_storage_types::sources::MzOffset;
use mz_storage_types::sources::load_generator::{
    Event, Generator, LoadGeneratorOutput, MarketingView,
};
use rand::{Rng, SeedableRng, distributions::Standard, rngs::SmallRng};

const CONTROL: &str = "control";
const EXPERIMENT: &str = "experiment";

pub struct Marketing {}

// Note that this generator issues retractions; if you change this,
// `mz_storage_types::sources::LoadGenerator::is_monotonic`
// must be updated.
impl Generator for Marketing {
    fn by_seed(
        &self,
        now: mz_ore::now::NowFn,
        seed: Option<u64>,
        _resume_offset: MzOffset,
    ) -> Box<(dyn Iterator<Item = (LoadGeneratorOutput, Event<Option<MzOffset>, (Row, Diff)>)>)>
    {
        let mut rng: SmallRng = SmallRng::seed_from_u64(seed.unwrap_or_default());

        let mut counter = 0;

        let mut future_updates = FutureUpdates::default();
        let mut pending: Vec<(MarketingView, Row, Diff)> = CUSTOMERS
            .into_iter()
            .enumerate()
            .map(|(id, email)| {
                let mut customer = Row::with_capacity(3);
                let mut packer = customer.packer();

                packer.push(Datum::Int64(id.try_into().unwrap()));
                packer.push(Datum::String(email));
                packer.push(Datum::Int64(rng.gen_range(5_000_000..10_000_000i64)));

                (MarketingView::Customers, customer, Diff::ONE)
            })
            .collect();

        let mut offset = 0;
        Box::new(
            iter::from_fn(move || {
                if pending.is_empty() {
                    let mut impression = Row::with_capacity(4);
                    let mut packer = impression.packer();

                    let impression_id = counter;
                    counter += 1;

                    packer.push(Datum::Int64(impression_id));
                    packer.push(Datum::Int64(
                        rng.gen_range(0..CUSTOMERS.len()).try_into().unwrap(),
                    ));
                    packer.push(Datum::Int64(rng.gen_range(0..20i64)));
                    let impression_time = now();
                    packer.push(Datum::TimestampTz(
                        to_datetime(impression_time)
                            .try_into()
                            .expect("timestamp must fit"),
                    ));

                    pending.push((MarketingView::Impressions, impression, Diff::ONE));

                    // 1 in 10 impressions have a click. Making us the
                    // most successful marketing organization in the world.
                    if rng.gen_range(0..10) == 1 {
                        let mut click = Row::with_capacity(2);
                        let mut packer = click.packer();

                        let click_time = impression_time + rng.gen_range(20000..40000);

                        packer.push(Datum::Int64(impression_id));
                        packer.push(Datum::TimestampTz(
                            to_datetime(click_time)
                                .try_into()
                                .expect("timestamp must fit"),
                        ));

                        future_updates
                            .insert(click_time, (MarketingView::Clicks, click, Diff::ONE));
                    }

                    let mut updates = future_updates.retrieve(now());
                    pending.append(&mut updates);

                    for _ in 0..rng.gen_range(1..2) {
                        let id = counter;
                        counter += 1;

                        let mut lead = Lead {
                            id,
                            customer_id: rng.gen_range(0..CUSTOMERS.len()).try_into().unwrap(),
                            created_at: now(),
                            converted_at: None,
                            conversion_amount: None,
                        };

                        pending.push((MarketingView::Leads, lead.to_row(), Diff::ONE));

                        // a highly scientific statistical model
                        // predicting the likelyhood of a conversion
                        let score = rng.sample::<f64, _>(Standard);
                        let label = score > 0.5f64;

                        let bucket = if lead.id % 10 <= 1 {
                            CONTROL
                        } else {
                            EXPERIMENT
                        };

                        let mut prediction = Row::with_capacity(4);
                        let mut packer = prediction.packer();

                        packer.push(Datum::Int64(lead.id));
                        packer.push(Datum::String(bucket));
                        packer.push(Datum::TimestampTz(
                            to_datetime(now()).try_into().expect("timestamp must fit"),
                        ));
                        packer.push(Datum::Float64(score.into()));

                        pending.push((MarketingView::ConversionPredictions, prediction, Diff::ONE));

                        let mut sent_coupon = false;
                        if !label && bucket == EXPERIMENT {
                            sent_coupon = true;
                            let amount = rng.gen_range(500..5000);

                            let mut coupon = Row::with_capacity(4);
                            let mut packer = coupon.packer();

                            let id = counter;
                            counter += 1;
                            packer.push(Datum::Int64(id));
                            packer.push(Datum::Int64(lead.id));
                            packer.push(Datum::TimestampTz(
                                to_datetime(now()).try_into().expect("timestamp must fit"),
                            ));
                            packer.push(Datum::Int64(amount));

                            pending.push((MarketingView::Coupons, coupon, Diff::ONE));
                        }

                        // Decide if a lead will convert. We assume our model is fairly
                        // accurate and correlates with conversions. We also assume
                        // that coupons make leads a little more liekly to convert.
                        let mut converted = rng.sample::<f64, _>(Standard) < score;
                        if sent_coupon && !converted {
                            converted = rng.sample::<f64, _>(Standard) < score;
                        }

                        if converted {
                            let converted_at = now() + rng.gen_range(1..30);

                            future_updates.insert(
                                converted_at,
                                (MarketingView::Leads, lead.to_row(), Diff::MINUS_ONE),
                            );

                            lead.converted_at = Some(converted_at);
                            lead.conversion_amount = Some(rng.gen_range(1000..25000));

                            future_updates.insert(
                                converted_at,
                                (MarketingView::Leads, lead.to_row(), Diff::ONE),
                            );
                        }
                    }
                }

                pending.pop().map(|(output, row, diff)| {
                    let msg = (
                        LoadGeneratorOutput::Marketing(output),
                        Event::Message(MzOffset::from(offset), (row, diff)),
                    );

                    let progress = if pending.is_empty() {
                        offset += 1;
                        Some((
                            LoadGeneratorOutput::Marketing(output),
                            Event::Progress(Some(MzOffset::from(offset))),
                        ))
                    } else {
                        None
                    };
                    std::iter::once(msg).chain(progress)
                })
            })
            .flatten(),
        )
    }
}

struct Lead {
    id: i64,
    customer_id: i64,
    created_at: u64,
    converted_at: Option<u64>,
    conversion_amount: Option<i64>,
}

impl Lead {
    fn to_row(&self) -> Row {
        let mut row = Row::with_capacity(5);
        let mut packer = row.packer();
        packer.push(Datum::Int64(self.id));
        packer.push(Datum::Int64(self.customer_id));
        packer.push(Datum::TimestampTz(
            to_datetime(self.created_at)
                .try_into()
                .expect("timestamp must fit"),
        ));
        packer.push(
            self.converted_at
                .map(|converted_at| {
                    Datum::TimestampTz(
                        to_datetime(converted_at)
                            .try_into()
                            .expect("timestamp must fit"),
                    )
                })
                .unwrap_or(Datum::Null),
        );
        packer.push(
            self.conversion_amount
                .map(Datum::Int64)
                .unwrap_or(Datum::Null),
        );

        row
    }
}

const CUSTOMERS: &[&str] = &[
    "andy.rooney@email.com",
    "marisa.tomei@email.com",
    "betty.thomas@email.com",
    "don.imus@email.com",
    "chevy.chase@email.com",
    "george.wendt@email.com",
    "oscar.levant@email.com",
    "jack.lemmon@email.com",
    "ben.vereen@email.com",
    "alexander.hamilton@email.com",
    "tommy.lee.jones@email.com",
    "george.takei@email.com",
    "norman.mailer@email.com",
    "casey.kasem@email.com",
    "sarah.miles@email.com",
    "john.landis@email.com",
    "george.c..marshall@email.com",
    "rita.coolidge@email.com",
    "al.unser@email.com",
    "ross.perot@email.com",
    "mikhail.gorbachev@email.com",
    "yasmine.bleeth@email.com",
    "darryl.strawberry@email.com",
    "bruce.springsteen@email.com",
    "weird.al.yankovic@email.com",
    "james.franco@email.com",
    "jean.smart@email.com",
    "stevie.nicks@email.com",
    "robert.merrill@email.com",
    "todd.bridges@email.com",
    "sam.cooke@email.com",
    "bert.convy@email.com",
    "erica.jong@email.com",
    "oscar.schindler@email.com",
    "douglas.fairbanks@email.com",
    "penny.marshall@email.com",
    "bram.stoker@email.com",
    "holly.hunter@email.com",
    "leontyne.price@email.com",
    "dick.smothers@email.com",
    "meredith.baxter@email.com",
    "carla.bruni@email.com",
    "joel.mccrea@email.com",
    "mariette.hartley@email.com",
    "vince.gill@email.com",
    "leon.schotter@email.com",
    "johann.von.goethe@email.com",
    "john.katz@email.com",
    "attenborough@email.com",
    "billy.graham@email.com",
];

#[derive(Default)]
struct FutureUpdates {
    updates: BTreeMap<u64, Vec<(MarketingView, Row, Diff)>>,
}

impl FutureUpdates {
    /// Schedules a row to be output at a certain time
    fn insert(&mut self, time: u64, update: (MarketingView, Row, Diff)) {
        match self.updates.entry(time) {
            Entry::Vacant(v) => {
                v.insert(vec![update]);
            }
            Entry::Occupied(o) => {
                o.into_mut().push(update);
            }
        }
    }

    /// Returns all rows that are scheduled to be output
    /// at or before a certain time.
    fn retrieve(&mut self, time: u64) -> Vec<(MarketingView, Row, Diff)> {
        let mut updates = vec![];
        while let Some(e) = self.updates.first_entry() {
            if *e.key() > time {
                break;
            }

            updates.append(&mut e.remove());
        }
        updates
    }
}
