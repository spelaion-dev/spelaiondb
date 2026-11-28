-- The tests' user, in the pluggable database FREEPDB1: user and password `spelaion`. Run by the
-- image on every start (`/opt/oracle/scripts/startup`), so it must work when the user exists.
alter session set container = FREEPDB1;
declare
  existing number;
begin
  select count(*) into existing from dba_users where username = 'SPELAION';
  if existing = 0 then
    -- The lite image has no USERS tablespace: the user's default one, without a quota.
    execute immediate 'create user spelaion identified by spelaion';
    execute immediate 'grant create session, create table, create view, create sequence, create procedure, create materialized view, unlimited tablespace to spelaion';
  end if;
  -- Outside the first creation, so a database made before it has it too: the read-only
  -- user's tests create scheduler jobs, synonyms and private database links as this user
  -- (tests/oracle.rs, ADR-080 and the ADR that supersedes its points 6 to 8).
  execute immediate 'grant create job, create synonym, create database link to spelaion';
end;
/
