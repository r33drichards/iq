#[macro_use]
extern crate rocket;

use dotenv::dotenv;
use redis::AsyncCommands;
use redis::Commands;
use rocket::State;
use rusoto_core::Region;
use rusoto_ec2::{Ec2, Ec2Client, RunInstancesRequest};
use std::env;
use tokio::sync::Mutex;
// Make sure to import NonZeroUsize at the top of your file
use std::num::NonZeroUsize;

struct AppState {
    ec2_client: Ec2Client,
    redis_url: String,
    desired_queue_size: usize,
    launch_template_id: String, // Launch Template ID for EC2 instances
}

#[get("/get_instance")]
async fn get_instance_id(state: &State<Mutex<AppState>>) -> Result<String, &'static str> {
    let state = state.lock().await;
    let client = redis::Client::open(state.redis_url.clone()).expect("Invalid Redis URL");
    let mut conn = client
        .get_async_connection()
        .await
        .expect("Failed to connect to Redis");

    let instance_id: Option<String> = conn
        .lpop("ec2_instance_queue", NonZeroUsize::new(1))
        .await
        .expect("Failed to pop from Redis");

    if let Some(id) = instance_id {
        // Asynchronously create and enqueue a new instance
        let current_size: usize = conn
            .llen("ec2_instance_queue")
            .await
            .expect("Failed to get queue length");
        if current_size < state.desired_queue_size {
            tokio::spawn(create_and_enqueue_ec2_instance(
                state.ec2_client.clone(),
                client,
                state.launch_template_id.clone(),
            ));
        }

        Ok(id)
    } else {
        Err("No instance ID available")
    }
}

async fn create_and_enqueue_ec2_instance(
    ec2_client: Ec2Client,
    redis_client: redis::Client,
    launch_template_id: String,
) {
    let request = RunInstancesRequest {
        launch_template: Some(rusoto_ec2::LaunchTemplateSpecification {
            launch_template_id: Some(launch_template_id),
            ..Default::default()
        }),
        max_count: 1,
        min_count: 1,
        ..Default::default()
    };
    let res = ec2_client
        .run_instances(request)
        .await
        .expect("Failed to create EC2 instance");

    if let Some(instances) = res.instances {
        for instance in instances {
            if let Some(instance_id) = instance.instance_id {
                let mut conn = redis_client
                    .get_async_connection()
                    .await
                    .expect("Failed to connect to Redis");
                let _: () = conn
                    .lpush("ec2_instance_queue", &instance_id)
                    .await
                    .expect("Failed to push to Redis");
            }
        }
    }
}

#[launch]
async fn rocket() -> _ {
    dotenv().ok();

    let redis_url = env::var("REDIS_URL").unwrap_or_else(|_| "redis://0.0.0.0:6379".to_string());
    let ec2_client = Ec2Client::new(Region::default());
    let desired_queue_size: i32 = env::var("DESIRED_QUEUE_SIZE")
        .unwrap_or_else(|_| "1".to_string())
        .parse()
        .expect("DESIRED_QUEUE_SIZE must be a valid number");
    let launch_template_id =
        env::var("LAUNCH_TEMPLATE_ID").unwrap_or_else(|_| "lt-0d7b76529ceabcb50".to_string());
    // todo seed queue to desired size if less than desired size
    // or reduce queue to desired size if greater than desired size
    let client = redis::Client::open(redis_url.clone()).expect("Invalid Redis URL");
    let mut conn = client.get_connection().expect("Failed to connect to Redis");
    let current_queue_size: i32 = conn
        .llen("ec2_instance_queue")
        .expect("Failed to get queue length");
    let instances_to_create = desired_queue_size - current_queue_size;

    match instances_to_create {
        0 => println!("Queue is already at desired size"),
        n if n > 0 => {
            println!(
                "Queue is smaller than desired size, creating {} instances",
                n
            );
            for _ in 0..instances_to_create {
                create_and_enqueue_ec2_instance(
                    ec2_client.clone(),
                    client.clone(),
                    launch_template_id.clone(),
                )
                .await;
            }
        }
        n if n < 0 => {
            println!(
                "Queue is larger than desired size, removing {} instances",
                n
            );
        }
        _ => println!("Queue is already at desired size"),
    }

    rocket::build()
        .manage(Mutex::new(AppState {
            ec2_client,
            redis_url,
            desired_queue_size: desired_queue_size.try_into().unwrap(),
            launch_template_id,
        }))
        .mount("/", routes![get_instance_id])
}
