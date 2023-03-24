import awswrangler as wr
import boto3

wr.config.s3_endpoint_url='http://s3-proxy.dev'
s = boto3.Session(profile_name='test',region_name='us-east-1')
a='s3://ops-9554/s3-proxy-test/dd/'
wr.s3.delete_objects(path=a,boto3_session=s)
