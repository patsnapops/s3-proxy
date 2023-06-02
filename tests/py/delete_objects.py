import awswrangler as wr
import boto3

wr.config.s3_endpoint_url='your_endpoint_url'
s = boto3.Session(profile_name='test',region_name='us-east-1')
a='s3://your_bucket_name/your_object_key'
wr.s3.delete_objects(path=a,boto3_session=s)
